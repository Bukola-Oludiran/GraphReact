//! `CommunicationGraph` — JS-facing wrapper.

use graphreact_core::CommunicationGraph;
use napi::Result;
use serde_json::Value;

use crate::conversion::{attrs_to_json, err_to_napi, json_to_attrs};

/// A `CommunicationGraph` exposed to JS as `Graph`.
#[napi(js_name = "Graph")]
pub struct JsCommunicationGraph {
    pub(crate) inner: CommunicationGraph,
}

/// `(source, target, key, attrs)` describing an edge.
#[napi(object, js_name = "EdgeTuple")]
pub struct JsEdgeTuple {
    pub source: String,
    pub target: String,
    pub key: String,
    pub attrs: Value,
}

#[napi]
impl JsCommunicationGraph {
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            inner: CommunicationGraph::new(),
        }
    }

    /// Number of nodes in the graph.
    #[napi(getter)]
    pub fn order(&self) -> u32 {
        u32::try_from(self.inner.order()).unwrap_or(u32::MAX)
    }

    /// Number of edges in the graph.
    #[napi(getter)]
    pub fn size(&self) -> u32 {
        u32::try_from(self.inner.size()).unwrap_or(u32::MAX)
    }

    /// All node IDs in insertion order.
    #[napi]
    pub fn nodes(&self) -> Vec<String> {
        self.inner.nodes().map(str::to_owned).collect()
    }

    #[napi]
    pub fn add_node(&mut self, id: String, attrs: Option<Value>) {
        self.inner
            .add_node(id, attrs.map(json_to_attrs).unwrap_or_default());
    }

    #[napi]
    pub fn has_node(&self, id: String) -> bool {
        self.inner.has_node(&id)
    }

    #[napi]
    pub fn remove_node(&mut self, id: String) -> Result<()> {
        self.inner.remove_node(&id).map_err(err_to_napi)
    }

    #[napi]
    pub fn get_node_data(&self, id: String) -> Result<Value> {
        self.inner
            .get_node_data(&id)
            .map(attrs_to_json)
            .map_err(err_to_napi)
    }

    #[napi]
    pub fn set_node_data(&mut self, id: String, attrs: Value) -> Result<()> {
        self.inner
            .set_node_data(&id, json_to_attrs(attrs))
            .map_err(err_to_napi)
    }

    #[napi]
    pub fn add_edge(
        &mut self,
        u: String,
        v: String,
        key: Option<String>,
        attrs: Option<Value>,
    ) -> String {
        self.inner
            .add_edge(u, v, key, attrs.map(json_to_attrs).unwrap_or_default())
    }

    #[napi]
    pub fn has_edge(&self, u: String, v: String, key: Option<String>) -> bool {
        self.inner.has_edge(&u, &v, key.as_deref())
    }

    #[napi]
    pub fn get_edge_data(&self, u: String, v: String, key: Option<String>) -> Result<Value> {
        self.inner
            .get_edge_data(&u, &v, key.as_deref())
            .map(attrs_to_json)
            .map_err(err_to_napi)
    }

    #[napi]
    pub fn set_edge_data(
        &mut self,
        u: String,
        v: String,
        key: Option<String>,
        attrs: Value,
    ) -> Result<()> {
        self.inner
            .set_edge_data(&u, &v, key.as_deref(), json_to_attrs(attrs))
            .map_err(err_to_napi)
    }

    #[napi]
    pub fn remove_edge(&mut self, u: String, v: String, key: Option<String>) -> Result<()> {
        self.inner
            .remove_edge(&u, &v, key.as_deref())
            .map_err(err_to_napi)
    }

    #[napi]
    pub fn edge_keys(&self, u: String, v: String) -> Vec<String> {
        self.inner.edge_keys(&u, &v)
    }

    #[napi]
    pub fn neighbors(&self, id: String) -> Result<Vec<String>> {
        self.inner.neighbors(&id).map_err(err_to_napi)
    }

    #[napi]
    pub fn degree(&self, id: String) -> Result<u32> {
        self.inner
            .degree(&id)
            .map(|d| u32::try_from(d).unwrap_or(u32::MAX))
            .map_err(err_to_napi)
    }

    #[napi]
    pub fn edges(&self) -> Vec<JsEdgeTuple> {
        self.inner
            .edges()
            .map(|(u, v, key)| {
                let attrs = self
                    .inner
                    .get_edge_data(&u, &v, Some(&key))
                    .map(attrs_to_json)
                    .unwrap_or(Value::Object(Default::default()));
                JsEdgeTuple {
                    source: u,
                    target: v,
                    key,
                    attrs,
                }
            })
            .collect()
    }

    #[napi]
    pub fn edges_of(&self, id: String) -> Result<Vec<JsEdgeTuple>> {
        let pairs = self.inner.edges_of(&id).map_err(err_to_napi)?;
        Ok(pairs
            .into_iter()
            .map(|(u, v, key)| {
                let attrs = self
                    .inner
                    .get_edge_data(&u, &v, Some(&key))
                    .map(attrs_to_json)
                    .unwrap_or(Value::Object(Default::default()));
                JsEdgeTuple {
                    source: u,
                    target: v,
                    key,
                    attrs,
                }
            })
            .collect())
    }
}

impl Default for JsCommunicationGraph {
    fn default() -> Self {
        Self::new()
    }
}
