use ipnet::{Contains, IpNet};
use linkerd2_app_core::{
    dns::Suffix,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    proxy::{api_resolve as api, resolve::recover},
    request_filter, Addr, Error, Recover,
};
use linkerd2_app_outbound::DiscoveryRejected;
use std::net::IpAddr;
use std::sync::Arc;
use tower_grpc::{generic::client::GrpcService, Body, BoxBody, Code, Status};

pub type Target = linkerd2_app_outbound::Concrete;

pub type Resolve<S> = request_filter::Service<
    PermitConfiguredDsts,
    recover::Resolve<BackoffUnlessInvalidArgument, api::Resolve<S>>,
>;

pub fn new<S>(
    service: S,
    suffixes: impl IntoIterator<Item = Suffix>,
    nets: impl IntoIterator<Item = IpNet>,
    token: &str,
    backoff: ExponentialBackoff,
) -> Resolve<S>
where
    S: GrpcService<BoxBody> + Clone + Send + 'static,
    S::ResponseBody: Send,
    <S::ResponseBody as Body>::Data: Send,
    S::Future: Send,
{
    request_filter::Service::new::<Target>(
        PermitConfiguredDsts::new(suffixes, nets),
        recover::Resolve::new::<Target>(
            backoff.into(),
            api::Resolve::new::<Target>(service).with_context_token(token),
        ),
    )
}

#[derive(Clone, Debug)]
pub struct PermitConfiguredDsts {
    name_suffixes: Arc<Vec<Suffix>>,
    networks: Arc<Vec<IpNet>>,
}

#[derive(Clone, Debug, Default)]
pub struct BackoffUnlessInvalidArgument(ExponentialBackoff);

// === impl PermitConfiguredDsts ===

impl PermitConfiguredDsts {
    fn new(
        name_suffixes: impl IntoIterator<Item = Suffix>,
        nets: impl IntoIterator<Item = IpNet>,
    ) -> Self {
        Self {
            name_suffixes: Arc::new(name_suffixes.into_iter().collect()),
            networks: Arc::new(nets.into_iter().collect()),
        }
    }
}

impl request_filter::RequestFilter<Target> for PermitConfiguredDsts {
    type Error = DiscoveryRejected;

    fn filter(&self, t: Target) -> Result<Target, Self::Error> {
        let permitted = match t.dst {
            Addr::Name(ref name) => self
                .name_suffixes
                .iter()
                .any(|suffix| suffix.contains(name.name())),
            Addr::Socket(sa) => self.networks.iter().any(|net| match (net, sa.ip()) {
                (IpNet::V4(net), IpAddr::V4(addr)) => net.contains(&addr),
                (IpNet::V6(net), IpAddr::V6(addr)) => net.contains(&addr),
                _ => false,
            }),
        };

        if permitted {
            Ok(t)
        } else {
            Err(DiscoveryRejected::new())
        }
    }
}

// === impl BackoffUnlessInvalidArgument ===

impl From<ExponentialBackoff> for BackoffUnlessInvalidArgument {
    fn from(eb: ExponentialBackoff) -> Self {
        BackoffUnlessInvalidArgument(eb)
    }
}

impl Recover<Error> for BackoffUnlessInvalidArgument {
    type Backoff = ExponentialBackoffStream;
    type Error = <ExponentialBackoffStream as futures::Stream>::Error;

    fn recover(&self, err: Error) -> Result<Self::Backoff, Error> {
        match err.downcast::<Status>() {
            Ok(ref status) if status.code() == Code::InvalidArgument => {
                tracing::debug!(message = "cannot recover", %status);
                return Err(DiscoveryRejected::new().into());
            }
            Ok(status) => tracing::trace!(message = "recovering", %status),
            Err(error) => tracing::trace!(message = "recovering", %error),
        }

        Ok(self.0.stream())
    }
}