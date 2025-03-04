use crate::{
    transport::{Payload, Transport},
    Call, Error, OffsetEncoding, Result,
};

use helix_core::{find_root, ChangeSet, Rope};
use jsonrpc_core as jsonrpc;
use lsp_types as lsp;
use serde_json::Value;
use std::future::Future;
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::{
    io::{BufReader, BufWriter},
    process::{Child, Command},
    sync::{
        mpsc::{channel, UnboundedReceiver, UnboundedSender},
        Notify, OnceCell,
    },
};

#[derive(Debug)]
pub struct Client {
    id: usize,
    _process: Child,
    server_tx: UnboundedSender<Payload>,
    request_counter: AtomicU64,
    pub(crate) capabilities: OnceCell<lsp::ServerCapabilities>,
    offset_encoding: OffsetEncoding,
    config: Option<Value>,
}

impl Client {
    #[allow(clippy::type_complexity)]
    pub fn start(
        cmd: &str,
        args: &[String],
        config: Option<Value>,
        id: usize,
    ) -> Result<(Self, UnboundedReceiver<(usize, Call)>, Arc<Notify>)> {
        let process = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // make sure the process is reaped on drop
            .kill_on_drop(true)
            .spawn();

        let mut process = process?;

        // TODO: do we need bufreader/writer here? or do we use async wrappers on unblock?
        let writer = BufWriter::new(process.stdin.take().expect("Failed to open stdin"));
        let reader = BufReader::new(process.stdout.take().expect("Failed to open stdout"));
        let stderr = BufReader::new(process.stderr.take().expect("Failed to open stderr"));

        let (server_rx, server_tx, initialize_notify) =
            Transport::start(reader, writer, stderr, id);

        let client = Self {
            id,
            _process: process,
            server_tx,
            request_counter: AtomicU64::new(0),
            capabilities: OnceCell::new(),
            offset_encoding: OffsetEncoding::Utf8,
            config,
        };

        Ok((client, server_rx, initialize_notify))
    }

    pub fn id(&self) -> usize {
        self.id
    }

    fn next_request_id(&self) -> jsonrpc::Id {
        let id = self.request_counter.fetch_add(1, Ordering::Relaxed);
        jsonrpc::Id::Num(id)
    }

    fn value_into_params(value: Value) -> jsonrpc::Params {
        use jsonrpc::Params;

        match value {
            Value::Null => Params::None,
            Value::Bool(_) | Value::Number(_) | Value::String(_) => Params::Array(vec![value]),
            Value::Array(vec) => Params::Array(vec),
            Value::Object(map) => Params::Map(map),
        }
    }

    pub fn is_initialized(&self) -> bool {
        self.capabilities.get().is_some()
    }

    pub fn capabilities(&self) -> &lsp::ServerCapabilities {
        self.capabilities
            .get()
            .expect("language server not yet initialized!")
    }

    pub fn offset_encoding(&self) -> OffsetEncoding {
        self.offset_encoding
    }

    /// Execute a RPC request on the language server.
    async fn request<R: lsp::request::Request>(&self, params: R::Params) -> Result<R::Result>
    where
        R::Params: serde::Serialize,
        R::Result: core::fmt::Debug, // TODO: temporary
    {
        // a future that resolves into the response
        let json = self.call::<R>(params).await?;
        let response = serde_json::from_value(json)?;
        Ok(response)
    }

    /// Execute a RPC request on the language server.
    fn call<R: lsp::request::Request>(
        &self,
        params: R::Params,
    ) -> impl Future<Output = Result<Value>>
    where
        R::Params: serde::Serialize,
    {
        let server_tx = self.server_tx.clone();
        let id = self.next_request_id();

        async move {
            use std::time::Duration;
            use tokio::time::timeout;

            let params = serde_json::to_value(params)?;

            let request = jsonrpc::MethodCall {
                jsonrpc: Some(jsonrpc::Version::V2),
                id,
                method: R::METHOD.to_string(),
                params: Self::value_into_params(params),
            };

            let (tx, mut rx) = channel::<Result<Value>>(1);

            server_tx
                .send(Payload::Request {
                    chan: tx,
                    value: request,
                })
                .map_err(|e| Error::Other(e.into()))?;

            // TODO: specifiable timeout, delay other calls until initialize success
            timeout(Duration::from_secs(20), rx.recv())
                .await
                .map_err(|_| Error::Timeout)? // return Timeout
                .ok_or(Error::StreamClosed)?
        }
    }

    /// Send a RPC notification to the language server.
    pub fn notify<R: lsp::notification::Notification>(
        &self,
        params: R::Params,
    ) -> impl Future<Output = Result<()>>
    where
        R::Params: serde::Serialize,
    {
        let server_tx = self.server_tx.clone();

        async move {
            let params = serde_json::to_value(params)?;

            let notification = jsonrpc::Notification {
                jsonrpc: Some(jsonrpc::Version::V2),
                method: R::METHOD.to_string(),
                params: Self::value_into_params(params),
            };

            server_tx
                .send(Payload::Notification(notification))
                .map_err(|e| Error::Other(e.into()))?;

            Ok(())
        }
    }

    /// Reply to a language server RPC call.
    pub fn reply(
        &self,
        id: jsonrpc::Id,
        result: core::result::Result<Value, jsonrpc::Error>,
    ) -> impl Future<Output = Result<()>> {
        use jsonrpc::{Failure, Output, Success, Version};

        let server_tx = self.server_tx.clone();

        async move {
            let output = match result {
                Ok(result) => Output::Success(Success {
                    jsonrpc: Some(Version::V2),
                    id,
                    result,
                }),
                Err(error) => Output::Failure(Failure {
                    jsonrpc: Some(Version::V2),
                    id,
                    error,
                }),
            };

            server_tx
                .send(Payload::Response(output))
                .map_err(|e| Error::Other(e.into()))?;

            Ok(())
        }
    }

    // -------------------------------------------------------------------------------------------
    // General messages
    // -------------------------------------------------------------------------------------------

    pub(crate) async fn initialize(&self) -> Result<lsp::InitializeResult> {
        // TODO: delay any requests that are triggered prior to initialize
        let root = find_root(None).and_then(|root| lsp::Url::from_file_path(root).ok());

        if self.config.is_some() {
            log::info!("Using custom LSP config: {}", self.config.as_ref().unwrap());
        }

        #[allow(deprecated)]
        let params = lsp::InitializeParams {
            process_id: Some(std::process::id()),
            // root_path is obsolete, use root_uri
            root_path: None,
            root_uri: root,
            initialization_options: self.config.clone(),
            capabilities: lsp::ClientCapabilities {
                text_document: Some(lsp::TextDocumentClientCapabilities {
                    completion: Some(lsp::CompletionClientCapabilities {
                        completion_item: Some(lsp::CompletionItemCapability {
                            snippet_support: Some(false),
                            ..Default::default()
                        }),
                        completion_item_kind: Some(lsp::CompletionItemKindCapability {
                            ..Default::default()
                        }),
                        context_support: None, // additional context information Some(true)
                        ..Default::default()
                    }),
                    hover: Some(lsp::HoverClientCapabilities {
                        // if not specified, rust-analyzer returns plaintext marked as markdown but
                        // badly formatted.
                        content_format: Some(vec![lsp::MarkupKind::Markdown]),
                        ..Default::default()
                    }),
                    code_action: Some(lsp::CodeActionClientCapabilities {
                        code_action_literal_support: Some(lsp::CodeActionLiteralSupport {
                            code_action_kind: lsp::CodeActionKindLiteralSupport {
                                value_set: [
                                    lsp::CodeActionKind::EMPTY,
                                    lsp::CodeActionKind::QUICKFIX,
                                    lsp::CodeActionKind::REFACTOR,
                                    lsp::CodeActionKind::REFACTOR_EXTRACT,
                                    lsp::CodeActionKind::REFACTOR_INLINE,
                                    lsp::CodeActionKind::REFACTOR_REWRITE,
                                    lsp::CodeActionKind::SOURCE,
                                    lsp::CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                                ]
                                .iter()
                                .map(|kind| kind.as_str().to_string())
                                .collect(),
                            },
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                window: Some(lsp::WindowClientCapabilities {
                    work_done_progress: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
            trace: None,
            workspace_folders: None,
            client_info: None,
            locale: None, // TODO
        };

        self.request::<lsp::request::Initialize>(params).await
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.request::<lsp::request::Shutdown>(()).await
    }

    pub fn exit(&self) -> impl Future<Output = Result<()>> {
        self.notify::<lsp::notification::Exit>(())
    }

    /// Tries to shut down the language server but returns
    /// early if server responds with an error.
    pub async fn shutdown_and_exit(&self) -> Result<()> {
        self.shutdown().await?;
        self.exit().await
    }

    /// Forcefully shuts down the language server ignoring any errors.
    pub async fn force_shutdown(&self) -> Result<()> {
        if let Err(e) = self.shutdown().await {
            log::warn!("language server failed to terminate gracefully - {}", e);
        }
        self.exit().await
    }

    // -------------------------------------------------------------------------------------------
    // Text document
    // -------------------------------------------------------------------------------------------

    pub fn text_document_did_open(
        &self,
        uri: lsp::Url,
        version: i32,
        doc: &Rope,
        language_id: String,
    ) -> impl Future<Output = Result<()>> {
        self.notify::<lsp::notification::DidOpenTextDocument>(lsp::DidOpenTextDocumentParams {
            text_document: lsp::TextDocumentItem {
                uri,
                language_id,
                version,
                text: String::from(doc),
            },
        })
    }

    pub fn changeset_to_changes(
        old_text: &Rope,
        new_text: &Rope,
        changeset: &ChangeSet,
        offset_encoding: OffsetEncoding,
    ) -> Vec<lsp::TextDocumentContentChangeEvent> {
        let mut iter = changeset.changes().iter().peekable();
        let mut old_pos = 0;
        let mut new_pos = 0;

        let mut changes = Vec::new();

        use crate::util::pos_to_lsp_pos;
        use helix_core::Operation::*;

        // this is dumb. TextEdit describes changes to the initial doc (concurrent), but
        // TextDocumentContentChangeEvent describes a series of changes (sequential).
        // So S -> S1 -> S2, meaning positioning depends on the previous edits.
        //
        // Calculation is therefore a bunch trickier.

        use helix_core::RopeSlice;
        fn traverse(pos: lsp::Position, text: RopeSlice) -> lsp::Position {
            let lsp::Position {
                mut line,
                mut character,
            } = pos;

            let mut chars = text.chars().peekable();
            while let Some(ch) = chars.next() {
                // LSP only considers \n, \r or \r\n as line endings
                if ch == '\n' || ch == '\r' {
                    // consume a \r\n
                    if ch == '\r' && chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    line += 1;
                    character = 0;
                } else {
                    character += ch.len_utf16() as u32;
                }
            }
            lsp::Position { line, character }
        }

        let old_text = old_text.slice(..);

        while let Some(change) = iter.next() {
            let len = match change {
                Delete(i) | Retain(i) => *i,
                Insert(_) => 0,
            };
            let mut old_end = old_pos + len;

            match change {
                Retain(i) => {
                    new_pos += i;
                }
                Delete(_) => {
                    let start = pos_to_lsp_pos(new_text, new_pos, offset_encoding);
                    let end = traverse(start, old_text.slice(old_pos..old_end));

                    // deletion
                    changes.push(lsp::TextDocumentContentChangeEvent {
                        range: Some(lsp::Range::new(start, end)),
                        text: "".to_string(),
                        range_length: None,
                    });
                }
                Insert(s) => {
                    let start = pos_to_lsp_pos(new_text, new_pos, offset_encoding);

                    new_pos += s.chars().count();

                    // a subsequent delete means a replace, consume it
                    let end = if let Some(Delete(len)) = iter.peek() {
                        old_end = old_pos + len;
                        let end = traverse(start, old_text.slice(old_pos..old_end));

                        iter.next();

                        // replacement
                        end
                    } else {
                        // insert
                        start
                    };

                    changes.push(lsp::TextDocumentContentChangeEvent {
                        range: Some(lsp::Range::new(start, end)),
                        text: s.into(),
                        range_length: None,
                    });
                }
            }
            old_pos = old_end;
        }

        changes
    }

    pub fn text_document_did_change(
        &self,
        text_document: lsp::VersionedTextDocumentIdentifier,
        old_text: &Rope,
        new_text: &Rope,
        changes: &ChangeSet,
    ) -> Option<impl Future<Output = Result<()>>> {
        // figure out what kind of sync the server supports

        let capabilities = self.capabilities.get().unwrap();

        let sync_capabilities = match capabilities.text_document_sync {
            Some(lsp::TextDocumentSyncCapability::Kind(kind))
            | Some(lsp::TextDocumentSyncCapability::Options(lsp::TextDocumentSyncOptions {
                change: Some(kind),
                ..
            })) => kind,
            // None | SyncOptions { changes: None }
            _ => return None,
        };

        let changes = match sync_capabilities {
            lsp::TextDocumentSyncKind::Full => {
                vec![lsp::TextDocumentContentChangeEvent {
                    // range = None -> whole document
                    range: None,        //Some(Range)
                    range_length: None, // u64 apparently deprecated
                    text: new_text.to_string(),
                }]
            }
            lsp::TextDocumentSyncKind::Incremental => {
                Self::changeset_to_changes(old_text, new_text, changes, self.offset_encoding)
            }
            lsp::TextDocumentSyncKind::None => return None,
        };

        Some(self.notify::<lsp::notification::DidChangeTextDocument>(
            lsp::DidChangeTextDocumentParams {
                text_document,
                content_changes: changes,
            },
        ))
    }

    pub fn text_document_did_close(
        &self,
        text_document: lsp::TextDocumentIdentifier,
    ) -> impl Future<Output = Result<()>> {
        self.notify::<lsp::notification::DidCloseTextDocument>(lsp::DidCloseTextDocumentParams {
            text_document,
        })
    }

    // will_save / will_save_wait_until

    pub fn text_document_did_save(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        text: &Rope,
    ) -> Option<impl Future<Output = Result<()>>> {
        let capabilities = self.capabilities.get().unwrap();

        let include_text = match &capabilities.text_document_sync {
            Some(lsp::TextDocumentSyncCapability::Options(lsp::TextDocumentSyncOptions {
                save: Some(options),
                ..
            })) => match options {
                lsp::TextDocumentSyncSaveOptions::Supported(true) => false,
                lsp::TextDocumentSyncSaveOptions::SaveOptions(lsp_types::SaveOptions {
                    include_text,
                }) => include_text.unwrap_or(false),
                // Supported(false)
                _ => return None,
            },
            // unsupported
            _ => return None,
        };

        Some(self.notify::<lsp::notification::DidSaveTextDocument>(
            lsp::DidSaveTextDocumentParams {
                text_document,
                text: include_text.then(|| text.into()),
            },
        ))
    }

    pub fn completion(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> impl Future<Output = Result<Value>> {
        // ) -> Result<Vec<lsp::CompletionItem>> {
        let params = lsp::CompletionParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            // TODO: support these tokens by async receiving and updating the choice list
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: None,
            },
            context: None,
            // lsp::CompletionContext { trigger_kind: , trigger_character: Some(), }
        };

        self.call::<lsp::request::Completion>(params)
    }

    pub fn text_document_signature_help(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> impl Future<Output = Result<Value>> {
        let params = lsp::SignatureHelpParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            context: None,
            // lsp::SignatureHelpContext
        };

        self.call::<lsp::request::SignatureHelpRequest>(params)
    }

    pub fn text_document_hover(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> impl Future<Output = Result<Value>> {
        let params = lsp::HoverParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            // lsp::SignatureHelpContext
        };

        self.call::<lsp::request::HoverRequest>(params)
    }

    // formatting

    pub fn text_document_formatting(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        options: lsp::FormattingOptions,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> Option<impl Future<Output = Result<Vec<lsp::TextEdit>>>> {
        let capabilities = self.capabilities.get().unwrap();

        // check if we're able to format
        match capabilities.document_formatting_provider {
            Some(lsp::OneOf::Left(true)) | Some(lsp::OneOf::Right(_)) => (),
            // None | Some(false)
            _ => return None,
        };
        // TODO: return err::unavailable so we can fall back to tree sitter formatting

        let params = lsp::DocumentFormattingParams {
            text_document,
            options,
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
        };

        let request = self.call::<lsp::request::Formatting>(params);

        Some(async move {
            let json = request.await?;
            let response: Option<Vec<lsp::TextEdit>> = serde_json::from_value(json)?;
            Ok(response.unwrap_or_default())
        })
    }

    pub async fn text_document_range_formatting(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        range: lsp::Range,
        options: lsp::FormattingOptions,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> anyhow::Result<Vec<lsp::TextEdit>> {
        let capabilities = self.capabilities.get().unwrap();

        // check if we're able to format
        match capabilities.document_range_formatting_provider {
            Some(lsp::OneOf::Left(true)) | Some(lsp::OneOf::Right(_)) => (),
            // None | Some(false)
            _ => return Ok(Vec::new()),
        };
        // TODO: return err::unavailable so we can fall back to tree sitter formatting

        let params = lsp::DocumentRangeFormattingParams {
            text_document,
            range,
            options,
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
        };

        let response = self
            .request::<lsp::request::RangeFormatting>(params)
            .await?;

        Ok(response.unwrap_or_default())
    }

    fn goto_request<
        T: lsp::request::Request<
            Params = lsp::GotoDefinitionParams,
            Result = Option<lsp::GotoDefinitionResponse>,
        >,
    >(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> impl Future<Output = Result<Value>> {
        let params = lsp::GotoDefinitionParams {
            text_document_position_params: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: None,
            },
        };

        self.call::<T>(params)
    }

    pub fn goto_definition(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> impl Future<Output = Result<Value>> {
        self.goto_request::<lsp::request::GotoDefinition>(text_document, position, work_done_token)
    }

    pub fn goto_type_definition(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> impl Future<Output = Result<Value>> {
        self.goto_request::<lsp::request::GotoTypeDefinition>(
            text_document,
            position,
            work_done_token,
        )
    }

    pub fn goto_implementation(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> impl Future<Output = Result<Value>> {
        self.goto_request::<lsp::request::GotoImplementation>(
            text_document,
            position,
            work_done_token,
        )
    }

    pub fn goto_reference(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        position: lsp::Position,
        work_done_token: Option<lsp::ProgressToken>,
    ) -> impl Future<Output = Result<Value>> {
        let params = lsp::ReferenceParams {
            text_document_position: lsp::TextDocumentPositionParams {
                text_document,
                position,
            },
            context: lsp::ReferenceContext {
                include_declaration: true,
            },
            work_done_progress_params: lsp::WorkDoneProgressParams { work_done_token },
            partial_result_params: lsp::PartialResultParams {
                partial_result_token: None,
            },
        };

        self.call::<lsp::request::References>(params)
    }

    pub fn document_symbols(
        &self,
        text_document: lsp::TextDocumentIdentifier,
    ) -> impl Future<Output = Result<Value>> {
        let params = lsp::DocumentSymbolParams {
            text_document,
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
            partial_result_params: lsp::PartialResultParams::default(),
        };

        self.call::<lsp::request::DocumentSymbolRequest>(params)
    }

    // empty string to get all symbols
    pub fn workspace_symbols(&self, query: String) -> impl Future<Output = Result<Value>> {
        let params = lsp::WorkspaceSymbolParams {
            query,
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
            partial_result_params: lsp::PartialResultParams::default(),
        };

        self.call::<lsp::request::WorkspaceSymbol>(params)
    }

    pub fn code_actions(
        &self,
        text_document: lsp::TextDocumentIdentifier,
        range: lsp::Range,
    ) -> impl Future<Output = Result<Value>> {
        let params = lsp::CodeActionParams {
            text_document,
            range,
            context: lsp::CodeActionContext::default(),
            work_done_progress_params: lsp::WorkDoneProgressParams::default(),
            partial_result_params: lsp::PartialResultParams::default(),
        };

        self.call::<lsp::request::CodeActionRequest>(params)
    }
}
