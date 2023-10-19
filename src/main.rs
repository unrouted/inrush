use anyhow::{Context, Result};
use askama::Template;
use crd::InrushGateway;
use futures::StreamExt;
use k8s_openapi::api::apps::v1::RollingUpdateDeployment;
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, SecretVolumeSource, Service, ServicePort, ServiceSpec,
};
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use k8s_openapi::{
    api::{
        apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy},
        core::v1::{Container, PodSpec, PodTemplateSpec, Volume, VolumeMount},
    },
    apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference},
};
use kube::runtime::reflector::store;
use kube::runtime::reflector::ObjectRef;
use kube::runtime::reflector::{reflector, Store};
use kube::runtime::WatchStreamExt;
use kube::CustomResourceExt;
use kube::{
    api::{Api, ObjectMeta, Patch, PatchParams},
    runtime::{
        controller::{Action, Config, Controller},
        watcher,
    },
    Client, Resource,
};
use resolv_conf;
use std::error::Error;
use std::fmt::Write;
use std::{collections::BTreeMap, sync::Arc};
use thiserror::Error as AnotherError;
use tokio::time::Duration;
use tracing::*;

mod crd;

struct Location {
    path: String,
    domain: String,
    port: i32,
}

struct InrushGatewayMetadata {
    locations: Vec<Location>,
}

#[derive(Template)]
#[template(path = "nginx.txt")]
struct IngressTemplate<'a> {
    metadata: &'a Arc<InrushGatewayMetadata>,
}

#[derive(Debug, AnotherError)]
enum ReconcileError {
    #[error("Failed to create ConfigMap: {0}")]
    ConfigMapCreationFailed(#[source] kube::Error),
    #[error("MissingObjectKey: {0}")]
    MissingObjectKey(&'static str),
    #[error("Error: {0}")]
    AnotherError(#[source] anyhow::Error),
}

async fn reconcile(ingress: Arc<InrushGateway>, ctx: Arc<Data>) -> anyhow::Result<Action> {
    ctx.ingress
        .wait_until_ready()
        .await
        .context("Waiting for ingress to be ready")?;

    let client = &ctx.client;

    let name = ingress
        .meta()
        .name
        .as_ref()
        .ok_or_else(|| ReconcileError::MissingObjectKey(".metadata.name"))?;
    let namespace = ingress
        .meta()
        .namespace
        .as_ref()
        .ok_or_else(|| ReconcileError::MissingObjectKey(".metadata.namespace"))?;
    let uid = ingress
        .meta()
        .uid
        .as_ref()
        .ok_or_else(|| ReconcileError::MissingObjectKey(".spec"))?;

    let owner_reference = OwnerReference {
        api_version: InrushGateway::api_version(&()).to_string(),
        kind: InrushGateway::kind(&()).to_string(),
        name: name.clone(),
        uid: uid.clone(),
        ..Default::default()
    };

    let config_map_name = format!("{name}-config");
    let deployment_name = format!("{name}-nginx");

    let mut locations = vec![];

    for ingress in ctx.ingress.state() {
        if let Some(spec) = &ingress.spec {
            if spec.ingress_class_name != Some(name.clone()) {
                continue;
            }

            if spec.default_backend.is_some() {
                warn!("InrushGateway {name}: Default backend is set. This does not make sense.");
                continue;
            }

            if let Some(rules) = &spec.rules {
                for rule in rules {
                    if let Some(http) = &rule.http {
                        for path in &http.paths {
                            if let Some(service) = &path.backend.service {
                                locations.push(Location {
                                    path: path.path.as_ref().unwrap().clone(),
                                    domain: format!(
                                        "{}.{}.{}",
                                        service.name,
                                        ingress.metadata.namespace.as_ref().unwrap(),
                                        ctx.cluster_domain
                                    ),
                                    port: service.port.as_ref().unwrap().number.unwrap(),
                                })
                            }
                        }
                    }
                }
            }
        }
    }

    let template = IngressTemplate {
        metadata: &Arc::new(InrushGatewayMetadata { locations }),
    };
    let rendered = template.render()?;

    let config_map = ConfigMap {
        metadata: ObjectMeta {
            name: Some(config_map_name.clone()),
            namespace: ingress.meta().namespace.clone(),
            owner_references: Some(vec![owner_reference.clone()]),
            labels: Some(BTreeMap::from([(
                "inrush.unrouted.uk/ingress".to_string(),
                name.clone(),
            )])),
            ..Default::default()
        },
        data: Some(BTreeMap::from([("nginx.conf".to_string(), rendered)])),
        ..Default::default()
    };

    println!("{:?}", config_map);

    let config_map_api = Api::<ConfigMap>::namespaced(client.clone(), namespace);
    config_map_api
        .patch(
            config_map
                .metadata
                .name
                .as_ref()
                .ok_or_else(|| ReconcileError::MissingObjectKey(".metadata.name"))?,
            &PatchParams::apply("inrush.unrouted.uk"),
            &Patch::Apply(&config_map),
        )
        .await
        .context("Failed to create or update config map")?;

    let deployment = Deployment {
        metadata: ObjectMeta {
            name: Some(deployment_name),
            namespace: ingress.meta().namespace.clone(),
            owner_references: Some(vec![owner_reference.clone()]),
            labels: Some(BTreeMap::from([(
                "inrush.unrouted.uk/ingress".to_string(),
                name.clone(),
            )])),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(3),
            strategy: Some(DeploymentStrategy {
                type_: Some("RollingUpdate".to_string()),
                rolling_update: Some(RollingUpdateDeployment {
                    max_surge: Some(IntOrString::Int(2)),
                    max_unavailable: Some(IntOrString::Int(1)),
                }),
            }),
            selector: LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    "inrush.unrouted.uk/ingress".to_string(),
                    name.clone(),
                )])),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(BTreeMap::from([(
                        "inrush.unrouted.uk/ingress".to_string(),
                        name.clone(),
                    )])),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    volumes: Some(vec![
                        Volume {
                            name: "config".to_string(),
                            config_map: Some(ConfigMapVolumeSource {
                                name: Some(config_map_name),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        Volume {
                            name: "tls".to_string(),
                            secret: Some(SecretVolumeSource {
                                secret_name: Some("tls".to_string()),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    ]),
                    containers: vec![Container {
                        name: "nginx".to_string(),
                        image: Some("nginx:stable".to_string()),
                        volume_mounts: Some(vec![
                            VolumeMount {
                                name: "config".to_string(),
                                mount_path: "/etc/nginx/nginx.conf".to_string(),
                                sub_path: Some("nginx.conf".to_string()),
                                read_only: Some(true),
                                ..Default::default()
                            },
                            VolumeMount {
                                name: "tls".to_string(),
                                mount_path: "/etc/nginx/tls".to_string(),
                                read_only: Some(true),
                                ..Default::default()
                            },
                        ]),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    let deployment_api = Api::<Deployment>::namespaced(client.clone(), namespace);
    deployment_api
        .patch(
            deployment
                .metadata
                .name
                .as_ref()
                .ok_or_else(|| ReconcileError::MissingObjectKey(".metadata.name"))?,
            &PatchParams::apply("inrush.unrouted.uk"),
            &Patch::Apply(&deployment),
        )
        .await
        .map_err(ReconcileError::ConfigMapCreationFailed)?;

    info!("Deployment updated");

    if let Some(templates) = &ingress.spec.service_templates {
        for template in templates {
            let svc_name = format!("{}-{}", name, template.name);

            let mut metadata = ObjectMeta {
                namespace: ingress.meta().namespace.clone(),
                name: Some(svc_name.clone()),
                owner_references: Some(vec![owner_reference.clone()]),
                ..Default::default()
            };

            if let Some(template_metadata) = &template.metadata {
                metadata.labels = template_metadata.labels.clone();
            }

            let mut spec = ServiceSpec {
                selector: Some(BTreeMap::from([(
                    "inrush.unrouted.uk/ingress".to_string(),
                    name.clone(),
                )])),
                ports: Some(vec![
                    ServicePort {
                        name: Some("http".to_string()),
                        protocol: Some("TCP".to_string()),
                        port: 80,
                        target_port: Some(IntOrString::Int(80)),
                        ..Default::default()
                    },
                    ServicePort {
                        name: Some("https".to_string()),
                        protocol: Some("TCP".to_string()),
                        port: 443,
                        target_port: Some(IntOrString::Int(443)),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            };

            if let Some(template_spec) = &template.spec {
                spec.type_ = template_spec.type_.clone();
                spec.load_balancer_class = template_spec.load_balancer_class.clone();
                spec.internal_traffic_policy = template_spec.internal_traffic_policy.clone();
                spec.external_traffic_policy = template_spec.external_traffic_policy.clone();
            }

            let service = Service {
                metadata,
                spec: Some(spec),
                ..Default::default()
            };

            let service_api = Api::<Service>::namespaced(client.clone(), namespace);
            service_api
                .patch(
                    &svc_name,
                    &PatchParams::apply("inrush.unrouted.uk").force(),
                    &Patch::Apply(&service),
                )
                .await
                .context("Error creating service")?;
        }
    }

    Ok(Action::requeue(Duration::from_secs(300)))
}

async fn reconcile_wrapper(
    inrushgateway: Arc<InrushGateway>,
    ctx: Arc<Data>,
) -> Result<Action, ReconcileError> {
    match reconcile(inrushgateway, ctx).await {
        Ok(result) => Ok(result),
        Err(e) => Err(ReconcileError::AnotherError(e)),
    }
}

/// The controller triggers this on reconcile errors
fn error_policy(
    _object: Arc<crd::InrushGateway>,
    _error: &ReconcileError,
    _ctx: Arc<Data>,
) -> Action {
    Action::requeue(Duration::from_secs(1))
}

// Data we want access to in error/reconcile calls
struct Data {
    cluster_domain: String,
    client: Client,
    ingress: Arc<Store<Ingress>>,
}

fn get_search_domain_from_resolv_conf() -> anyhow::Result<Option<String>> {
    let contents = std::fs::read_to_string("/etc/resolv.conf")?;
    let cfg = resolv_conf::Config::parse(&contents)?;

    if let Some(domains) = cfg.get_search() {
        for domain in domains {
            if domain.starts_with(&"svc.") {
                return Ok(Some(domain.clone()));
            }
        }
    }

    Ok(None)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    info!(
        "Creating crd: {}",
        serde_yaml::to_string(&InrushGateway::crd())?
    );

    let cluster_domain = match std::env::var("INRUSH_SERVICE_DOMAIN") {
        Ok(cluster_domain) => Ok(cluster_domain),
        Err(_) => match get_search_domain_from_resolv_conf() {
            Ok(Some(cluster_domain)) => Ok(cluster_domain),
            Ok(None) => Ok("svc.cluster.local".to_string()),
            Err(e) => Err(e),
        },
    }?;

    let client = Client::try_default().await?;

    let inrushgateways = Api::<InrushGateway>::all(client.clone());
    let ingress = Api::<Ingress>::all(client.clone());
    let deployments = Api::<Deployment>::all(client.clone());
    let configmaps = Api::<ConfigMap>::all(client.clone());

    // limit the controller to running a maximum of two concurrent reconciliations
    let config = Config::default().concurrency(2);

    let (ingress_reader, ingress_writer) = store();
    let rf = reflector(ingress_writer, watcher(ingress, watcher::Config::default()));
    let ingress_stream = rf.applied_objects();

    Controller::new(inrushgateways, watcher::Config::default())
        .watches_stream(ingress_stream, |ingress| match &ingress.spec {
            Some(spec) => spec.ingress_class_name.as_ref().map(|ingress_class_name| {
                ObjectRef::new(ingress_class_name)
                    .within(ingress.clone().meta().namespace.as_ref().unwrap())
            }),
            None => None,
        })
        .owns(deployments, watcher::Config::default())
        .owns(configmaps, watcher::Config::default())
        .with_config(config)
        .shutdown_on_signal()
        .run(
            reconcile_wrapper,
            error_policy,
            Arc::new(Data {
                cluster_domain,
                client,
                ingress: Arc::new(ingress_reader),
            }),
        )
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("reconciled {:?}", o),
                Err(err) => {
                    let mut msg = err.to_string();
                    let mut source = err.source();
                    while let Some(src) = source {
                        writeln!(msg, ": {src}").unwrap();
                        source = src.source();
                    }
                    error!("reconcile failed: {}", msg);
                }
            }
        })
        .await;
    info!("controller terminated");
    Ok(())
}
