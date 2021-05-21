use crate::{http, stack_labels, tcp, trace_labels, Config, Outbound};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    detect, errors, http_tracing, io, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    svc::{self, stack::Param},
    tls,
    transport::{OrigDstAddr, Remote, ServerAddr},
    AddrMatch, Error, NameAddr,
};
use thiserror::Error;
use tracing::{debug_span, info_span};

#[derive(Clone)]
struct AllowHttpProfile(AddrMatch);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Http {
    target: Target,
    version: http::Version,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Target {
    Forward(OrigDstAddr),
    Override(NameAddr),
}

#[derive(Debug, Error)]
#[error("ingress-mode routing requires a service profile")]
struct ProfileRequired;

#[derive(Debug, Default, Error)]
#[error("ingress-mode routing is HTTP-only")]
struct IngressHttpOnly;

#[derive(Debug, Default, Error)]
#[error("l5d-dst-override is not a valid host:port")]
struct InvalidOverrideHeader;

const DST_OVERRIDE_HEADER: &str = "l5d-dst-override";

// === impl Outbound ===

impl Outbound<svc::BoxNewHttp<http::Endpoint>> {
    /// Routes HTTP requests according to the l5d-dst-override header.
    ///
    /// This is only intended for Ingress configurations, where we assume all
    /// outbound traffic is HTTP.
    pub fn into_ingress<T, I, P, R>(
        self,
        profiles: P,
        resolve: R,
    ) -> svc::BoxNewService<T, svc::BoxService<I, (), Error>>
    where
        T: Param<OrigDstAddr> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
        P::Error: Send,
        P::Future: Send,
        R: Clone + Send + Sync + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R::Resolution: Send,
        R::Future: Send + Unpin,
    {
        let Outbound {
            config,
            runtime: rt,
            stack: http_logical,
        } = self.clone().push_http_logical(resolve);

        let http_endpoint = self.into_inner();

        let Config {
            allow_discovery,
            proxy:
                ProxyConfig {
                    server: ServerConfig { h2_settings, .. },
                    dispatch_timeout,
                    max_in_flight_requests,
                    detect_protocol_timeout,
                    buffer_capacity,
                    cache_max_idle_age,
                    ..
                },
            ..
        } = config;
        let profile_domains = allow_discovery.names().clone();

        // Route requests with destinations that can be discovered via the `l5d-dst-override` header
        // through the logical (load balanced) stack and route requests without the
        // `l5d-dst-override` header through the endpoint stack.
        http_logical
            .push_switch(
                |(profile, Http { target, version }): (Option<profiles::Receiver>, _)| {
                    // If the target did not include an override header, build an endpoint stack
                    // with the original destination address (ignoring all headers, etc).
                    if let Target::Forward(OrigDstAddr(addr)) = target {
                        return Ok(svc::Either::B(http::Endpoint {
                            addr: Remote(ServerAddr(addr)),
                            metadata: Metadata::default(),
                            logical_addr: None,
                            protocol: version,
                            opaque_protocol: false,
                            tls: tls::ConditionalClientTls::None(
                                tls::NoClientTls::IngressWithoutOverride,
                            ),
                        }));
                    }

                    // Otherwise, if a profile was discovered, use it to build a logical stack.
                    if let Some(profile) = profile {
                        let addr = profile.borrow().addr.clone();
                        if let Some(logical_addr) = addr {
                            return Ok(svc::Either::A(http::Logical {
                                profile,
                                logical_addr,
                                protocol: version,
                            }));
                        }
                    }

                    // Otherwise, the override header was present but no profile information could
                    // be discovered, so fail the request.
                    Err(ProfileRequired)
                },
                http_endpoint,
            )
            .push(profiles::discover::layer(profiles, move |h: Http| {
                // Lookup the profile if the override header was set and it is in the configured
                // profile domains. Otherwise, profile discovery is skipped.
                if let Target::Override(dst) = h.target {
                    if profile_domains.matches(dst.name()) {
                        return Ok(profiles::LookupAddr(dst.into()));
                    }
                }

                tracing::debug!(
                    domains = %profile_domains,
                    "Address not in a configured domain",
                );
                Err(profiles::DiscoveryRejected::new(
                    "not in configured ingress search addresses",
                ))
            }))
            // This service is buffered because it needs to initialize the profile resolution and a
            // fail-fast is instrumented in case it becomes unavailable. When this service is in
            // fail-fast, ensure that we drive the inner service to readiness even if new requests
            // aren't received.
            .push_on_response(
                svc::layers()
                    .push(rt.metrics.stack.layer(stack_labels("http", "logical")))
                    .push(svc::layer::mk(svc::SpawnReady::new))
                    .push(svc::FailFast::layer("HTTP Logical", dispatch_timeout))
                    .push_spawn_buffer(buffer_capacity),
            )
            .push_cache(cache_max_idle_age)
            .push_on_response(
                svc::layers()
                    .push(http::strip_header::request::layer(DST_OVERRIDE_HEADER))
                    .push(http::Retain::layer()),
            )
            .instrument(|h: &Http| match h.target {
                Target::Forward(_) => info_span!("forward"),
                Target::Override(ref dst) => info_span!("override", %dst),
            })
            .push(svc::BoxNewService::layer())
            // Obtain a new inner service for each request (fom the above cache).
            .push(svc::NewRouter::layer(
                |http::Accept { orig_dst, protocol }| {
                    move |req: &http::Request<_>| {
                        // Use either the override header or the original destination address.
                        let target = match http::authority_from_header(req, DST_OVERRIDE_HEADER) {
                            None => Target::Forward(orig_dst),
                            Some(a) => {
                                let dst = NameAddr::from_authority_with_default_port(&a, 80)
                                    .map_err(|_| InvalidOverrideHeader)?;
                                Target::Override(dst)
                            }
                        };
                        Ok(Http {
                            target,
                            version: protocol,
                        })
                    }
                },
            ))
            .push(http::NewNormalizeUri::layer())
            .push_on_response(
                svc::layers()
                    .push(http::MarkAbsoluteForm::layer())
                    // The concurrency-limit can force the service into fail-fast, but it need not
                    // be driven to readiness on a background task (i.e., by `SpawnReady`).
                    // Otherwise, the inner service is always ready (because it's a router).
                    .push(svc::ConcurrencyLimit::layer(max_in_flight_requests))
                    .push(svc::FailFast::layer("Ingress server", dispatch_timeout))
                    .push(rt.metrics.http_errors.clone())
                    .push(errors::layer())
                    .push(http_tracing::server(rt.span_sink, trace_labels()))
                    .push(http::BoxResponse::layer())
                    .push(http::BoxRequest::layer()),
            )
            .instrument(|a: &http::Accept| debug_span!("http", v = %a.protocol))
            .push(http::NewServeHttp::layer(h2_settings, rt.drain))
            .push_request_filter(|(http, accept): (Option<http::Version>, _)| {
                http.map(|h| http::Accept::from((h, accept)))
                    .ok_or(IngressHttpOnly)
            })
            .push_cache(cache_max_idle_age)
            .push_map_target(detect::allow_timeout)
            .push(svc::BoxNewService::layer())
            .push(detect::NewDetectService::layer(
                detect_protocol_timeout,
                http::DetectHttp::default(),
            ))
            .push(rt.metrics.transport.layer_accept())
            .instrument(|a: &tcp::Accept| info_span!("ingress", orig_dst = %a.orig_dst))
            .push_map_target(|a: T| {
                let orig_dst = Param::<OrigDstAddr>::param(&a);
                tcp::Accept::from(orig_dst)
            })
            .push_on_response(svc::BoxService::layer())
            .push(svc::BoxNewService::layer())
            .check_new_service::<T, I>()
            .into_inner()
    }
}
