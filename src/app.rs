// Copyright (c) 2019 Jason White
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.
use std::io;
use std::sync::Arc;

use futures::{
    future::{self, Either},
    Future, IntoFuture, Stream,
};
use http::{
    self, header,
    uri::{Authority, Scheme},
    StatusCode, Uri,
};
use hyper::{self, service::Service, Chunk, Method, Request, Response};

use crate::error::Error;
use crate::hyperext::{into_request, Body, IntoResponse, RequestExt};
use crate::lfs;
use crate::storage::{LFSObject, Namespace, Storage, StorageKey};

/// Shared state for all instances of the `App` service.
pub struct State<S> {
    // Storage backend.
    storage: S,
}

impl<S> State<S> {
    pub fn new(storage: S) -> Self {
        State { storage }
    }
}

#[derive(Clone)]
pub struct App<S> {
    state: Arc<State<S>>,
}

impl<S> App<S>
where
    S: Storage + Send + Sync + 'static,
    S::Error: Into<Error> + 'static,
    Error: From<S::Error>,
{
    pub fn new(state: Arc<State<S>>) -> Self {
        App { state }
    }

    /// Generates a "404 not found" response.
    fn not_found(
        &mut self,
        _req: Request<Body>,
    ) -> Result<Response<Body>, Error> {
        Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body("Not found".into())?)
    }

    /// Handles `/api` routes.
    fn api(
        &mut self,
        req: Request<Body>,
    ) -> impl Future<Item = Response<Body>, Error = Error> {
        let mut parts = req.uri().path().split('/').filter(|s| !s.is_empty());

        // Skip over the '/api' part.
        assert_eq!(parts.next(), Some("api"));

        // Extract the namespace.
        let namespace = match (parts.next(), parts.next()) {
            (Some(org), Some(project)) => {
                Namespace::new(org.into(), project.into())
            }
            _ => {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(Body::from("Missing org/project in URL"))
                    .map_err(Into::into)
                    .response();
            }
        };

        match parts.next() {
            Some("object") => {
                // Upload or download a single object.
                let oid = parts.next().and_then(|x| x.parse::<lfs::Oid>().ok());
                let oid = match oid {
                    Some(oid) => oid,
                    None => {
                        return Response::builder()
                            .status(StatusCode::BAD_REQUEST)
                            .body(Body::from("Missing OID parameter."))
                            .map_err(Into::into)
                            .response();
                    }
                };

                let key = StorageKey::new(oid).with_namespace(namespace);

                match *req.method() {
                    Method::GET => self.download(req, key).response(),
                    Method::PUT => self.upload(req, key).response(),
                    _ => self.not_found(req).response(),
                }
            }
            Some("objects") => match (req.method(), parts.next()) {
                (&Method::POST, Some("batch")) => {
                    self.batch(req, namespace).response()
                }
                (&Method::POST, Some("verify")) => {
                    self.verify(req, namespace).response()
                }
                _ => self.not_found(req).response(),
            },
            _ => self.not_found(req).response(),
        }
    }

    /// Downloads a single LFS object.
    fn download(
        &mut self,
        _req: Request<Body>,
        key: StorageKey,
    ) -> impl Future<Item = Response<Body>, Error = Error> {
        self.state.storage.get(&key).from_err::<Error>().and_then(
            move |object| -> Result<_, Error> {
                if let Some(object) = object {
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header(
                            header::CONTENT_TYPE,
                            "application/octet-stream",
                        )
                        .header(header::CONTENT_LENGTH, object.len())
                        .body(Body::wrap_stream(object.stream()))
                        .map_err(Into::into);
                } else {
                    return Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Body::empty())
                        .map_err(Into::into);
                }
            },
        )
    }

    /// Uploads a single LFS object.
    fn upload(
        &mut self,
        req: Request<Body>,
        key: StorageKey,
    ) -> <Self as Service>::Future {
        let len = req
            .headers()
            .get("Content-Length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());

        let len = match len {
            Some(len) => len,
            None => {
                return Box::new(
                    Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(Body::from("Invalid Content-Length header."))
                        .map_err(Into::into)
                        .into_future(),
                );
            }
        };

        // Verify the SHA256 of the uploaded object as it is being uploaded.
        let stream = req
            .into_body()
            .map(Chunk::into_bytes)
            .from_err::<Error>()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e));

        let object = LFSObject::new(len, Box::new(stream));

        Box::new(
            self.state
                .storage
                .put(key, object)
                .from_err::<Error>()
                .and_then(|_| {
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::empty())
                        .map_err(Into::into)
                }),
        )
    }

    /// Verifies that an LFS object exists on the server.
    fn verify(
        &mut self,
        req: Request<Body>,
        namespace: Namespace,
    ) -> impl Future<Item = Response<Body>, Error = Error> {
        let state = self.state.clone();

        req.into_body()
            .into_json()
            .and_then(move |val: lfs::VerifyRequest| {
                let key = StorageKey::new(val.oid).with_namespace(namespace);

                state.storage.size(&key).from_err::<Error>().and_then(
                    move |size| {
                        if let Some(size) = size {
                            if size == val.size {
                                return Response::builder()
                                    .status(StatusCode::OK)
                                    .body(Body::empty())
                                    .map_err(Into::into);
                            }
                        }

                        // Object doesn't exist or the size is incorrect.
                        Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .body(Body::empty())
                            .map_err(Into::into)
                    },
                )
            })
    }

    /// Batch API endpoint for the Git LFS server spec.
    ///
    /// See also:
    /// https://github.com/git-lfs/git-lfs/blob/master/docs/api/batch.md
    fn batch(
        &mut self,
        req: Request<Body>,
        namespace: Namespace,
    ) -> impl Future<Item = Response<Body>, Error = Error> {
        // Get the host name and scheme.
        let uri = Uri::builder()
            .scheme(req.scheme().unwrap_or(Scheme::HTTP))
            .authority(
                req.authority()
                    .unwrap_or_else(|| Authority::from_static("localhost")),
            )
            .path_and_query("/")
            .build()
            .unwrap();

        let state = self.state.clone();

        req.into_body().into_json().then(
            move |result: Result<lfs::BatchRequest, _>| {
                match result {
                    Ok(val) => {
                        let operation = val.operation;

                        // For each object, check if it exists in the storage
                        // backend.
                        let objects =
                            val.objects.into_iter().map(move |object| {
                                let uri = uri.clone();

                                let key = StorageKey::new(object.oid)
                                    .with_namespace(namespace.clone());

                                let namespace = namespace.clone();

                                state.storage.size(&key).map(move |size| {
                                    basic_response(
                                        uri, object, operation, size, namespace,
                                    )
                                })
                            });

                        Either::A(
                            future::join_all(objects)
                                .from_err::<Error>()
                                .and_then(|objects| {
                                    let response = lfs::BatchResponse {
                                        transfer: Some(lfs::Transfer::Basic),
                                        objects,
                                    };

                                    Response::builder()
                                        .status(StatusCode::OK)
                                        .header(
                                            header::CONTENT_TYPE,
                                            "application/json",
                                        )
                                        .body(Body::json(&response)?)
                                        .map_err(Into::into)
                                }),
                        )
                    }
                    Err(err) => {
                        let response = lfs::BatchResponseError {
                            message: err.to_string(),
                            documentation_url: None,
                            request_id: None,
                        };

                        Either::B(
                            Response::builder()
                                .status(StatusCode::BAD_REQUEST)
                                .body(Body::json(&response).unwrap())
                                .map_err(Into::into)
                                .into_future(),
                        )
                    }
                }
            },
        )
    }
}

fn basic_response(
    uri: Uri,
    object: lfs::RequestObject,
    op: lfs::Operation,
    size: Option<u64>,
    namespace: Namespace,
) -> lfs::ResponseObject {
    if let Some(size) = size {
        // Ensure that the client and server agree on the size of the object.
        if object.size != size {
            return lfs::ResponseObject {
                oid: object.oid,
                size,
                error: Some(lfs::ObjectError {
                    code: 400,
                    message: format!(
                        "bad object size: requested={}, actual={}",
                        object.size, size
                    ),
                }),
                authenticated: None,
                actions: None,
            };
        }
    }

    let href = format!("{}api/{}/object/{}", uri, namespace, object.oid);

    let action = lfs::Action {
        href,
        header: None,
        expires_in: None,
        expires_at: None,
    };

    match op {
        lfs::Operation::Upload => {
            // If the object does exist, then we should not return any action.
            //
            // If the object does not exist, then we should return an upload
            // action.
            match size {
                Some(size) => lfs::ResponseObject {
                    oid: object.oid,
                    size,
                    error: None,
                    authenticated: Some(true),
                    actions: None,
                },
                None => lfs::ResponseObject {
                    oid: object.oid,
                    size: object.size,
                    error: None,
                    authenticated: Some(true),
                    actions: Some(lfs::Actions {
                        download: None,
                        upload: Some(action.clone()),
                        verify: Some(lfs::Action {
                            href: format!(
                                "{}api/{}/objects/verify",
                                uri, namespace
                            ),
                            header: None,
                            expires_in: None,
                            expires_at: None,
                        }),
                    }),
                },
            }
        }
        lfs::Operation::Download => {
            // If the object does not exist, then we should return a 404 error
            // for this object.
            match size {
                Some(size) => lfs::ResponseObject {
                    oid: object.oid,
                    size,
                    error: None,
                    authenticated: Some(true),
                    actions: Some(lfs::Actions {
                        download: Some(action),
                        upload: None,
                        verify: None,
                    }),
                },
                None => lfs::ResponseObject {
                    oid: object.oid,
                    size: object.size,
                    error: Some(lfs::ObjectError {
                        code: 404,
                        message: "not found".into(),
                    }),
                    authenticated: Some(true),
                    actions: None,
                },
            }
        }
    }
}

impl<S> Service for App<S>
where
    S: Storage + Send + Sync + 'static,
    S::Error: Into<Error> + 'static,
    Error: From<S::Error>,
{
    type ReqBody = hyper::Body;
    type ResBody = Body;
    type Error = Error;
    type Future = Box<dyn Future<Item = Response<Body>, Error = Error> + Send>;

    fn call(&mut self, req: Request<Self::ReqBody>) -> Self::Future {
        let req = into_request(req);

        if req.uri().path().starts_with("/api/") {
            return self.api(req).response();
        }

        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .map_err(Into::into)
            .response()
    }
}
