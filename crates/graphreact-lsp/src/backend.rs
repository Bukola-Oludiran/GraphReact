//! LSP backend wiring.
//!
//! Implements [`tower_lsp_server::LanguageServer`] on
//! [`GrxLanguageServer`]. Each handler is a thin shim that delegates
//! to the topic modules:
//!
//! - `initialize` / `initialized` / `shutdown` / `did_open` /
//!   `did_change` / `did_close` — lifecycle, served from this file.
//! - Diagnostics — published on every parse via [`crate::diagnostics`].
//! - Hover — [`crate::hover`].
//! - Goto-definition — [`crate::resolve`].
//! - Completion — [`crate::completion`].

use std::path::Path;

use graphreact_grx_parser::parser::Parser;
use tower_lsp_server::jsonrpc::Result as JsonRpcResult;
use tower_lsp_server::ls_types::{
    CompletionOptions, CompletionParams, CompletionResponse, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverParams, HoverProviderCapability, InitializeParams,
    InitializeResult, InitializedParams, Location, MessageType, OneOf, PositionEncodingKind,
    Range, ServerCapabilities, ServerInfo, TextDocumentSyncCapability, TextDocumentSyncKind,
    Uri,
};
use tower_lsp_server::{Client, LanguageServer};

use crate::diagnostics::from_parse_error;
use crate::documents::{path_to_uri_string, uri_to_path, DocumentStore};
use crate::resolve::{resolve_import_alias, resolve_subsystem_target, resolve_with_store};

/// The LSP backend. Holds the document store + the JSON-RPC client
/// handle so handlers can push notifications (diagnostics, log
/// messages) back to the editor.
#[derive(Debug)]
pub struct GrxLanguageServer {
    client: Client,
    documents: DocumentStore,
}

impl GrxLanguageServer {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: DocumentStore::new(),
        }
    }

    /// Test-helper: snapshot the in-memory documents.
    pub fn documents(&self) -> &DocumentStore {
        &self.documents
    }

    async fn refresh_diagnostics(&self, uri: &Uri) {
        let Some(entry) = self.documents.get(uri).await else {
            return;
        };
        let diagnostics = match Parser::parse_file(&entry.text) {
            Ok(file) => {
                // Run the IR builder + style desugarer too — those
                // surface additional errors the parse stage doesn't
                // catch.
                match graphreact_grx_parser::ir::build_ir(&file) {
                    Ok(_) => Vec::new(),
                    Err(err) => vec![from_parse_error(&err, &entry.text, uri)],
                }
            }
            Err(err) => vec![from_parse_error(&err, &entry.text, uri)],
        };
        self.client
            .publish_diagnostics(uri.clone(), diagnostics, Some(entry.version))
            .await;
    }
}

impl LanguageServer for GrxLanguageServer {
    async fn initialize(&self, _params: InitializeParams) -> JsonRpcResult<InitializeResult> {
        let capabilities = ServerCapabilities {
            // We can serve full-text sync today; incremental sync is
            // a Phase-1B follow-on (would wire through
            // `IncrementalParser::reparse`).
            text_document_sync: Some(TextDocumentSyncCapability::Kind(
                TextDocumentSyncKind::FULL,
            )),
            position_encoding: Some(PositionEncodingKind::UTF8),
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            definition_provider: Some(OneOf::Left(true)),
            completion_provider: Some(CompletionOptions {
                resolve_provider: Some(false),
                trigger_characters: Some(vec![
                    "<".to_string(),
                    " ".to_string(),
                    ":".to_string(),
                    "\"".to_string(),
                ]),
                all_commit_characters: None,
                work_done_progress_options: Default::default(),
                completion_item: None,
            }),
            ..ServerCapabilities::default()
        };
        Ok(InitializeResult {
            capabilities,
            server_info: Some(ServerInfo {
                name: "graphreact-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            // clangd-style "UTF-8 offsets" extension. Newer LSP
            // clients honour `position_encoding` (above); older ones
            // honour `offset_encoding` — set both so the byte-level
            // SourceLoc math stays correct across the client
            // ecosystem.
            offset_encoding: Some("utf-8".to_string()),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(
                MessageType::INFO,
                "graphreact-lsp ready (M23 outer-shell MVP)",
            )
            .await;
    }

    async fn shutdown(&self) -> JsonRpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let td = params.text_document;
        self.documents
            .upsert(&td.uri, td.text, td.version)
            .await;
        self.refresh_diagnostics(&td.uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let version = params.text_document.version;
        // Full-sync MVP: take the last change's text in full.
        let new_text = params
            .content_changes
            .into_iter()
            .last()
            .map(|c| c.text)
            .unwrap_or_default();
        self.documents.upsert(&uri, new_text, version).await;
        self.refresh_diagnostics(&uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.remove(&params.text_document.uri).await;
        // Clear stale diagnostics.
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
    }

    async fn hover(&self, params: HoverParams) -> JsonRpcResult<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri.clone();
        let pos = params.text_document_position_params.position;
        let Some(entry) = self.documents.get(&uri).await else {
            return Ok(None);
        };
        let Ok(file) = Parser::parse_file(&entry.text) else {
            return Ok(None);
        };
        Ok(crate::hover::build_hover(&file, &entry.text, pos))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> JsonRpcResult<Option<GotoDefinitionResponse>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .clone();
        let pos = params.text_document_position_params.position;
        let Some(entry) = self.documents.get(&uri).await else {
            return Ok(None);
        };
        let Ok(file) = Parser::parse_file(&entry.text) else {
            return Ok(None);
        };

        // Classify the cursor: are we on a Subsystem ref, an import
        // alias-qualified tag, or neither?
        let target = crate::hover::classify_hover_target(&file, &entry.text, pos);

        match target {
            Some(crate::hover::HoverTarget::SubsystemRef(ref_value)) => {
                let Some(path) = uri_to_path(&uri) else {
                    return Ok(None);
                };
                let Ok(resolved) = resolve_with_store(&path, &self.documents).await else {
                    return Ok(None);
                };
                let Some(target) = resolve_subsystem_target(&resolved, &ref_value) else {
                    return Ok(None);
                };
                let Some(target_uri_str) = path_to_uri_string(&target.file_path) else {
                    return Ok(None);
                };
                let Ok(target_uri) = target_uri_str.parse::<Uri>() else {
                    return Ok(None);
                };
                let target_source = self
                    .documents
                    .get_by_path(&target.file_path)
                    .await
                    .map(|d| d.text)
                    .unwrap_or_else(|| {
                        std::fs::read_to_string(&target.file_path).unwrap_or_default()
                    });
                let pos = crate::position::loc_to_position(target.loc, &target_source);
                Ok(Some(GotoDefinitionResponse::Scalar(Location {
                    uri: target_uri,
                    range: Range::new(pos, pos),
                })))
            }
            Some(crate::hover::HoverTarget::Tag(tag)) => {
                // Aliased tag form: `mcu:Counter` (rare per the
                // tag-attr surface in the discovery scoping). Detect
                // by looking for ':' inside the captured word.
                if let Some(colon_idx) = tag.find(':') {
                    let (alias, _name) = tag.split_at(colon_idx);
                    let Some(path) = uri_to_path(&uri) else {
                        return Ok(None);
                    };
                    let Ok(resolved) = resolve_with_store(&path, &self.documents).await else {
                        return Ok(None);
                    };
                    let Some(target) = resolve_import_alias(&resolved, alias) else {
                        return Ok(None);
                    };
                    let Some(target_uri_str) = path_to_uri_string(&target.file_path) else {
                        return Ok(None);
                    };
                    let Ok(target_uri) = target_uri_str.parse::<Uri>() else {
                        return Ok(None);
                    };
                    let pos = tower_lsp_server::ls_types::Position::new(0, 0);
                    return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                        uri: target_uri,
                        range: Range::new(pos, pos),
                    })));
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> JsonRpcResult<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri.clone();
        let pos = params.text_document_position.position;
        let Some(entry) = self.documents.get(&uri).await else {
            return Ok(None);
        };
        // Parsing may fail mid-edit (we WANT completions on broken
        // input). Fall back to an empty file so the static tag/style
        // catalogs still complete.
        let file = Parser::parse_file(&entry.text).unwrap_or_default();
        let items = crate::completion::build_completions(&file, &entry.text, pos);
        if items.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CompletionResponse::Array(items)))
        }
    }
}

/// Construction-time helper that builds a fresh server bound to
/// `client`. Bin uses [`tower_lsp_server::LspService::new`] which
/// expects a function from `Client` to `Backend` — this is that.
#[must_use]
pub fn build_backend(client: Client) -> GrxLanguageServer {
    GrxLanguageServer::new(client)
}

/// Test-only helper: synthesise a `MultiFileResolved`-driven goto
/// without needing the wire layer. The integration tests use this so
/// they don't have to spin a full stdio LSP transport.
#[doc(hidden)]
pub async fn run_resolve_subsystem(
    root: &Path,
    ref_value: &str,
    store: &DocumentStore,
) -> Option<crate::resolve::SubsystemTarget> {
    let resolved = resolve_with_store(root, store).await.ok()?;
    resolve_subsystem_target(&resolved, ref_value)
}
