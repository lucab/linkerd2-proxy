use futures::{future, Future, Poll};
use http;
use http::header::{TRANSFER_ENCODING, HeaderValue};
use std::marker::PhantomData;

use super::{h1, settings::Host, Settings};
use svc;

const L5D_ORIG_PROTO: &str = "l5d-orig-proto";

/// Upgrades HTTP requests from their original protocol to HTTP2.
#[derive(Clone, Debug)]
pub struct Upgrade<S> {
    inner: S,
}

/// Downgrades HTTP2 requests that were previousl upgraded to their original
/// protocol.
#[derive(Clone, Debug)]
pub struct Downgrade<S> {
    inner: S,
}

pub fn detect<B>(req: &http::Request<B>) -> Settings {
    if req.version() == http::Version::HTTP_2 {
        if let Some(orig_proto) = req.headers().get(L5D_ORIG_PROTO) {
            trace!("detected orig-proto: {:?}", orig_proto);
            let val = orig_proto.as_bytes();
            let was_absolute_form = was_absolute_form(val);
            let host = Host::detect(req);

            return Settings::Http1 {
                host,
                is_h1_upgrade: false, // orig-proto is never used with upgrades
                was_absolute_form,
            };
        }
    }

    Settings::detect(req)
}

#[derive(Debug)]
pub struct LayerUpgrade<T>(PhantomData<fn() -> T>);

#[derive(Debug)]
pub struct LayerDowngrade<T>(PhantomData<fn() -> T>);

pub struct MakeUpgrade<T, M>
where
    M: svc::Make<T>,
{
    inner: M,
    _p: PhantomData<fn() -> T>,
}

pub struct MakeDowngrade<T, M>
where
    M: svc::Make<T>,
{
    inner: M,
    _p: PhantomData<fn() -> T>,
}

// ==== impl Upgrade =====

pub fn upgrade<T>() -> LayerUpgrade<T> {
    LayerUpgrade(PhantomData)
}

impl<T> Clone for LayerUpgrade<T> {
    fn clone(&self) -> Self {
        LayerUpgrade(PhantomData)
    }
}

impl<T, M, A, B> svc::Layer<T, T, M> for LayerUpgrade<T>
where
    M: svc::Make<T>,
    M::Value: svc::Service<Request = http::Request<A>, Response = http::Response<B>>,
{
    type Value = <MakeUpgrade<T, M> as svc::Make<T>>::Value;
    type Error = <MakeUpgrade<T, M> as svc::Make<T>>::Error;
    type Make = MakeUpgrade<T, M>;

    fn bind(&self, inner: M) -> Self::Make {
        MakeUpgrade { inner, _p: PhantomData }
    }
}

impl<T, M, A, B> Clone for MakeUpgrade<T, M>
where
    M: svc::Make<T> + Clone,
    M::Value: svc::Service<Request = http::Request<A>, Response = http::Response<B>>,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _p: PhantomData,
        }
    }
}

impl<T, M, A, B> svc::Make<T> for MakeUpgrade<T, M>
where
    M: svc::Make<T>,
    M::Value: svc::Service<Request = http::Request<A>, Response = http::Response<B>>,
{
    type Value = Upgrade<M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(&target)?;
        Ok(Upgrade { inner })
    }
}

impl<S, A, B> svc::Service for Upgrade<S>
where
    S: svc::Service<Request = http::Request<A>, Response = http::Response<B>>,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = future::Map<
        S::Future,
        fn(S::Response) -> S::Response
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut req: Self::Request) -> Self::Future {
        let mut downgrade_response = false;

        if req.version() != http::Version::HTTP_2 {
            debug!("upgrading {:?} to HTTP2 with orig-proto", req.version());

            // absolute-form is far less common, origin-form is the usual,
            // so only encode the extra information if it's different than
            // the normal.
            let was_absolute_form = h1::is_absolute_form(req.uri());
            if !was_absolute_form {
                // Since the version is going to set to HTTP_2, the NormalizeUri
                // middleware won't normalize the URI automatically, so it
                // needs to be done now.
                h1::normalize_our_view_of_uri(&mut req);
            }

            let val = match (req.version(), was_absolute_form) {
                (http::Version::HTTP_11, false) => "HTTP/1.1",
                (http::Version::HTTP_11, true) => "HTTP/1.1; absolute-form",
                (http::Version::HTTP_10, false) => "HTTP/1.0",
                (http::Version::HTTP_10, true) => "HTTP/1.0; absolute-form",
                (v, _) => unreachable!("bad orig-proto version: {:?}", v),
            };
            req.headers_mut().insert(
                L5D_ORIG_PROTO,
                HeaderValue::from_static(val)
            );

            // transfer-encoding is illegal in HTTP2
            req.headers_mut().remove(TRANSFER_ENCODING);

            *req.version_mut() = http::Version::HTTP_2;
            downgrade_response = true;
        }

        let fut = self.inner.call(req);

        if downgrade_response {
            fut.map(|mut res| {
                debug_assert_eq!(res.version(), http::Version::HTTP_2);
                let version = if let Some(orig_proto) = res.headers().get(L5D_ORIG_PROTO) {
                    debug!("downgrading {} response: {:?}", L5D_ORIG_PROTO, orig_proto);
                    if orig_proto == "HTTP/1.1" {
                        http::Version::HTTP_11
                    } else if orig_proto == "HTTP/1.0" {
                        http::Version::HTTP_10
                    } else {
                        warn!("unknown {} header value: {:?}", L5D_ORIG_PROTO, orig_proto);
                        res.version()
                    }
                } else {
                    res.version()
                };
                *res.version_mut() = version;
                res
            })
        } else {
            // Just passing through...
            fut.map(|res| res)
        }
    }
}

// ===== impl Downgrade =====

pub fn downgrade<T>() -> LayerDowngrade<T> {
    LayerDowngrade(PhantomData)
}

impl<T> Clone for LayerDowngrade<T> {
    fn clone(&self) -> Self {
        LayerDowngrade(PhantomData)
    }
}

impl<T, M, A, B> svc::Layer<T, T, M> for LayerDowngrade<T>
where
    M: svc::Make<T>,
    M::Value: svc::Service<Request = http::Request<A>, Response = http::Response<B>>,
{
    type Value = <MakeDowngrade<T, M> as svc::Make<T>>::Value;
    type Error = <MakeDowngrade<T, M> as svc::Make<T>>::Error;
    type Make = MakeDowngrade<T, M>;

    fn bind(&self, inner: M) -> Self::Make {
        MakeDowngrade { inner, _p: PhantomData }
    }
}

impl<T, M, A, B> Clone for MakeDowngrade<T, M>
where
    M: svc::Make<T> + Clone,
    M::Value: svc::Service<Request = http::Request<A>, Response = http::Response<B>>,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _p: PhantomData,
        }
    }
}

impl<T, M, A, B> svc::Make<T> for MakeDowngrade<T, M>
where
    M: svc::Make<T>,
    M::Value: svc::Service<Request = http::Request<A>, Response = http::Response<B>>,
{
    type Value = Downgrade<M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(&target)?;
        Ok(Downgrade { inner })
    }
}

impl<S, A, B> svc::Service for Downgrade<S>
where
    S: svc::Service<Request = http::Request<A>, Response = http::Response<B>>,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = future::Map<
        S::Future,
        fn(S::Response) -> S::Response
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut req: Self::Request) -> Self::Future {
        let mut upgrade_response = false;

        if req.version() == http::Version::HTTP_2 {
            if let Some(orig_proto) = req.headers_mut().remove(L5D_ORIG_PROTO) {
                debug!("translating HTTP2 to orig-proto: {:?}", orig_proto);

                let val: &[u8] = orig_proto.as_bytes();

                if val.starts_with(b"HTTP/1.1") {
                    *req.version_mut() = http::Version::HTTP_11;
                } else if val.starts_with(b"HTTP/1.0") {
                    *req.version_mut() = http::Version::HTTP_10;
                } else {
                    warn!(
                        "unknown {} header value: {:?}",
                        L5D_ORIG_PROTO,
                        orig_proto,
                    );
                }

                if !was_absolute_form(val) {
                    h1::set_origin_form(req.uri_mut());
                }
                upgrade_response = true;
            }
        }

        let fut = self.inner.call(req);

        if upgrade_response {
            fut.map(|mut res| {
                let orig_proto = if res.version() == http::Version::HTTP_11 {
                    "HTTP/1.1"
                } else if res.version() == http::Version::HTTP_10 {
                    "HTTP/1.0"
                } else {
                    return res;
                };

                res.headers_mut().insert(
                    L5D_ORIG_PROTO,
                    HeaderValue::from_static(orig_proto)
                );

                // transfer-encoding is illegal in HTTP2
                res.headers_mut().remove(TRANSFER_ENCODING);

                *res.version_mut() = http::Version::HTTP_2;
                res
            })
        } else {
            fut.map(|res| res)
        }
    }
}

fn was_absolute_form(val: &[u8]) -> bool {
    val.len() >= "HTTP/1.1; absolute-form".len()
        && &val[10..23] == b"absolute-form"
}

