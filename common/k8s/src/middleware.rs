use std::collections::HashMap;
use std::env;

use parking_lot::Mutex;
use regex::Regex;

use http::types::body::{KeyValueMap, LineBuilder};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::{ListParams, Resource, WatchEvent},
    client::APIClient,
    config,
    runtime::Informer,
    Api,
};
use metrics::Metrics;
use middleware::{Middleware, Status};

use futures::stream::StreamExt;
use tokio::runtime::{Builder, Runtime};

use crate::errors::K8sError;
use std::convert::TryFrom;

lazy_static! {
    static ref K8S_REG: Regex = Regex::new(
        r#"^/var/log/containers/([a-z0-9A-Z\-.]+)_([a-z0-9A-Z\-.]+)_([a-z0-9A-Z\-.]+)-([a-z0-9]{64}).log$"#
    ).unwrap_or_else(|e| panic!("K8S_REG Regex::new() failed: {}", e));
}

quick_error! {
    #[derive(Debug)]
    enum Error {
        Io(e: std::io::Error) {
            from()
            display("{}", e)
        }
        Utf(e: std::string::FromUtf8Error) {
            from()
            display("{}", e)
        }
        Regex {
            from()
            display("failed to parse path")
        }
        K8s(e: kube::Error) {
            from()
            display("{}", e)
        }
    }
}

pub struct K8sMiddleware {
    metadata: Mutex<HashMap<String, PodMetadata>>,
    informer: Mutex<Informer<Pod>>,
    runtime: Mutex<Option<Runtime>>,
}

impl K8sMiddleware {
    pub fn new() -> Self {
        let mut runtime = Builder::new()
            .threaded_scheduler()
            .enable_all()
            .core_threads(2)
            .build()
            .unwrap_or_else(|e| panic!("unable to build tokio runtime: {}", e));
        let this = runtime.block_on(async {
            let node = env::var("NODE_NAME").expect("unable to read environment variable NODE_NAME");

            let config = config::incluster_config().unwrap_or_else(|e| panic!("unable to get cluster configuration info: {}", e));
            let client = APIClient::new(config);

            let params = ListParams::default().fields(&format!("spec.nodeName={}", node));
            let mut metadata = HashMap::new();

            match Api::<Pod>::all(client.clone()).list(&params).await {
                Ok(pods) => {
                    for pod in pods {
                        let pod_meta_data = match PodMetadata::try_from(pod) {
                            Ok(v) => v,
                            Err(e) => {
                                warn!("ignoring pod on initialization: {}", e);
                                continue;
                            }
                        };
                        metadata.insert(
                            format!("{}_{}", pod_meta_data.name, pod_meta_data.namespace),
                            pod_meta_data,
                        );
                    }
                },
                Err(e) => {
                    warn!("unable to poll pods during initialization: {}", e);
                }
            }

            K8sMiddleware {
                metadata: Mutex::new(metadata),
                informer: Mutex::new(Informer::new(client, params, Resource::all::<Pod>())),
                runtime: Mutex::new(None),
            }
        });

        *this.runtime.lock() = Some(runtime);
        this
    }

    fn handle_pod(&self, event: WatchEvent<Pod>) {
        match event {
            WatchEvent::Added(pod) => {
                let pod_meta_data = match PodMetadata::try_from(pod) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("ignoring pod added event: {}", e);
                        return;
                    }
                };
                self.metadata.lock().insert(
                    format!("{}_{}", pod_meta_data.name, pod_meta_data.namespace),
                    pod_meta_data,
                );
                Metrics::k8s().increment_creates();
            }
            WatchEvent::Modified(pod) => {
                let new_pod_meta_data = match PodMetadata::try_from(pod) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("ignoring pod modified event: {}", e);
                        return;
                    }
                };
                if let Some(old_pod_meta_data) = self.metadata.lock().get_mut(&format!(
                    "{}_{}",
                    new_pod_meta_data.name,
                    new_pod_meta_data.namespace
                )) {
                    old_pod_meta_data.labels = new_pod_meta_data.labels;
                    old_pod_meta_data.annotations = new_pod_meta_data.annotations;
                }
            }
            WatchEvent::Deleted(pod) => {
                let pod_meta_data = match PodMetadata::try_from(pod) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("ignoring pod deleted event: {}", e);
                        return;
                    }
                };
                self.metadata.lock().remove(&format!(
                    "{}_{}",
                    pod_meta_data.name,
                    pod_meta_data.namespace
                ));
                Metrics::k8s().increment_deletes();
            }
            WatchEvent::Error(e) => {
                debug!("kubernetes api error event: {:?}", e);
            }
        }
    }
}

impl Middleware for K8sMiddleware {
    fn run(&self) {
        let mut runtime = self.runtime.lock().take().expect("tokio runtime not initialized");
        let informer = self.informer.lock();

        runtime.block_on(async move {
            loop {
                let mut pods = match informer.poll().await {
                    Ok(v) => v.boxed(),
                    Err(e) => {
                        error!("unable to poll kubernetes api for pods: {}", e);
                        continue;
                    }
                };
                Metrics::k8s().increment_polls();

                while let Some(Ok(event)) = pods.next().await {
                    self.handle_pod(event);
                }
            }
        });
    }

    fn process(&self, lines: Vec<LineBuilder>) -> Status {
        let mut container_line = None;
        for line in lines.iter() {
            if let Some(ref file_name) = line.file {
                if let Some((name, namespace)) = parse_container_path(&file_name) {
                    if let Some(pod_meta_data) =
                        self.metadata.lock().get(&format!("{}_{}", name, namespace))
                    {
                        Metrics::k8s().increment_lines();
                        let mut new_line = line.clone();
                        new_line = new_line.labels(pod_meta_data.labels.clone());
                        new_line = new_line.annotations(pod_meta_data.annotations.clone());
                        container_line = Some(new_line);
                    }
                }
            }
        }

        if let Some(line) = container_line {
            return Status::Ok(vec![line]);
        }

        Status::Ok(lines)
    }
}

impl TryFrom<k8s_openapi::api::core::v1::Pod> for PodMetadata {
    type Error = K8sError;

    fn try_from(value: k8s_openapi::api::core::v1::Pod) -> Result<Self, Self::Error> {
        let real_pod_meta = match value.metadata {
            Some(v) => v,
            None => {
                return Err(K8sError::PodMissingMetaError("metadata"));
            }
        };

        let name = match real_pod_meta.name {
            Some(v) => v,
            None => {
                return Err(K8sError::PodMissingMetaError("metadata.name"));
            }
        };
        let namespace = match real_pod_meta.namespace {
            Some(v) => v,
            None => {
                return Err(K8sError::PodMissingMetaError("metadata.namespace"));
            }
        };

        Ok(PodMetadata {
            name: name,
            namespace: namespace,
            labels: real_pod_meta.labels.map_or_else(|| KeyValueMap::new(), |v| v.into()),
            annotations: real_pod_meta.annotations.map_or_else(|| KeyValueMap::new(), |v| v.into())
        })
    }
}

fn parse_container_path(path: &str) -> Option<(String, String)> {
    let captures = K8S_REG.captures(path)?;
    Some((
        captures.get(1)?.as_str().into(),
        captures.get(2)?.as_str().into(),
    ))
}

struct PodMetadata {
    name: String,
    namespace: String,
    labels: KeyValueMap,
    annotations: KeyValueMap,
}