//! NetworkX-parity algorithm wrappers — exposed to JS as free functions
//! that take a `Graph` instance.

use graphreact_algorithms as algo;
use napi::Result;

use crate::conversion::err_to_napi;
use crate::graph::JsCommunicationGraph;

fn depth_limit(value: Option<u32>) -> Option<usize> {
    value.map(|v| v as usize)
}

#[napi]
pub fn bfs_nodes(
    graph: &JsCommunicationGraph,
    source: String,
    depth_limit_value: Option<u32>,
) -> Result<Vec<String>> {
    algo::bfs_nodes(&graph.inner, &source, depth_limit(depth_limit_value)).map_err(err_to_napi)
}

#[napi]
pub fn bfs_edges(
    graph: &JsCommunicationGraph,
    source: String,
    depth_limit_value: Option<u32>,
) -> Result<Vec<Vec<String>>> {
    let pairs = algo::bfs_edges(&graph.inner, &source, depth_limit(depth_limit_value))
        .map_err(err_to_napi)?;
    Ok(pairs.into_iter().map(|(u, v)| vec![u, v]).collect())
}

#[napi]
pub fn dfs_nodes(
    graph: &JsCommunicationGraph,
    source: String,
    depth_limit_value: Option<u32>,
) -> Result<Vec<String>> {
    algo::dfs_nodes(&graph.inner, &source, depth_limit(depth_limit_value)).map_err(err_to_napi)
}

#[napi]
pub fn dfs_edges(
    graph: &JsCommunicationGraph,
    source: String,
    depth_limit_value: Option<u32>,
) -> Result<Vec<Vec<String>>> {
    let pairs = algo::dfs_edges(&graph.inner, &source, depth_limit(depth_limit_value))
        .map_err(err_to_napi)?;
    Ok(pairs.into_iter().map(|(u, v)| vec![u, v]).collect())
}

#[napi]
pub fn connected_components(graph: &JsCommunicationGraph) -> Result<Vec<Vec<String>>> {
    algo::connected_components(&graph.inner)
        .map(|components| {
            components
                .into_iter()
                .map(|set| {
                    let mut v: Vec<String> = set.into_iter().collect();
                    v.sort();
                    v
                })
                .collect()
        })
        .map_err(err_to_napi)
}

#[napi]
pub fn is_connected(graph: &JsCommunicationGraph) -> Result<bool> {
    algo::is_connected(&graph.inner).map_err(err_to_napi)
}

#[napi]
pub fn number_of_connected_components(graph: &JsCommunicationGraph) -> Result<u32> {
    algo::number_of_connected_components(&graph.inner)
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
        .map_err(err_to_napi)
}

#[napi]
pub fn has_cycle(graph: &JsCommunicationGraph) -> Result<bool> {
    algo::has_cycle(&graph.inner).map_err(err_to_napi)
}

#[napi]
pub fn find_cycle(graph: &JsCommunicationGraph) -> Result<Option<Vec<Vec<String>>>> {
    let cycle = algo::find_cycle(&graph.inner).map_err(err_to_napi)?;
    Ok(cycle.map(|edges| edges.into_iter().map(|(u, v)| vec![u, v]).collect()))
}

#[napi]
pub fn are_connected(graph: &JsCommunicationGraph, u: String, v: String) -> bool {
    algo::are_connected(&graph.inner, &u, &v)
}

#[napi]
pub fn shortest_path(
    graph: &JsCommunicationGraph,
    source: String,
    target: String,
) -> Result<Option<Vec<String>>> {
    algo::shortest_path(
        &graph.inner,
        &source,
        &target,
        &algo::PathOptions::default(),
    )
    .map_err(err_to_napi)
}
