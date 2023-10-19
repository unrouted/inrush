use std::collections::BTreeMap;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Default, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceTemplateMeta {
    pub labels: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceTemplateSpec {
    pub load_balancer_class: Option<String>,
    pub external_traffic_policy: Option<String>,
    pub internal_traffic_policy: Option<String>,
    #[serde(rename = "type")]
    pub type_: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceTemplate {
    pub name: String,
    pub metadata: Option<ServiceTemplateMeta>,
    pub spec: Option<ServiceTemplateSpec>,
}

#[derive(CustomResource, Debug, Serialize, Deserialize, Default, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[kube(
    group = "inrush.unrouted.uk",
    version = "v1",
    kind = "InrushGateway",
    namespaced
)]
pub struct InrushGatewaySpec {
    pub service_templates: Option<Vec<ServiceTemplate>>,
}
