mod client;
mod transport;

pub use client::Client;
pub use futures_executor::block_on;
pub use jsonrpc::Call;
pub use jsonrpc_core as jsonrpc;
pub use lsp::{Position, Url};
pub use lsp_types as lsp;

use futures_util::stream::select_all::SelectAll;
use helix_core::syntax::LanguageConfiguration;

use std::{
    collections::{hash_map::Entry, HashMap},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_stream::wrappers::UnboundedReceiverStream;

pub type Result<T> = core::result::Result<T, Error>;
type LanguageId = String;

#[derive(Error, Debug)]
pub enum Error {
    #[error("protocol error: {0}")]
    Rpc(#[from] jsonrpc::Error),
    #[error("failed to parse: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("IO Error: {0}")]
    IO(#[from] std::io::Error),
    #[error("request timed out")]
    Timeout,
    #[error("server closed the stream")]
    StreamClosed,
    #[error("LSP not defined")]
    LspNotDefined,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum OffsetEncoding {
    /// UTF-8 code units aka bytes
    #[serde(rename = "utf-8")]
    Utf8,
    /// UTF-16 code units
    #[serde(rename = "utf-16")]
    Utf16,
}

pub mod util {
    use super::*;
    use helix_core::{Range, Rope, Transaction};

    /// Converts [`lsp::Position`] to a position in the document.
    ///
    /// Returns `None` if position exceeds document length or an operation overflows.
    pub fn lsp_pos_to_pos(
        doc: &Rope,
        pos: lsp::Position,
        offset_encoding: OffsetEncoding,
    ) -> Option<usize> {
        let max_line = doc.lines().count().saturating_sub(1);
        let pos_line = pos.line as usize;
        let pos_line = if pos_line > max_line {
            return None;
        } else {
            pos_line
        };
        match offset_encoding {
            OffsetEncoding::Utf8 => {
                let max_char = doc
                    .line_to_char(max_line)
                    .checked_add(doc.line(max_line).len_chars())?;
                let line = doc.line_to_char(pos_line);
                let pos = line.checked_add(pos.character as usize)?;
                if pos <= max_char {
                    Some(pos)
                } else {
                    None
                }
            }
            OffsetEncoding::Utf16 => {
                let max_char = doc
                    .line_to_char(max_line)
                    .checked_add(doc.line(max_line).len_chars())?;
                let max_cu = doc.char_to_utf16_cu(max_char);
                let line = doc.line_to_char(pos_line);
                let line_start = doc.char_to_utf16_cu(line);
                let pos = line_start.checked_add(pos.character as usize)?;
                if pos <= max_cu {
                    Some(doc.utf16_cu_to_char(pos))
                } else {
                    None
                }
            }
        }
    }

    /// Converts position in the document to [`lsp::Position`].
    ///
    /// Panics when `pos` is out of `doc` bounds or operation overflows.
    pub fn pos_to_lsp_pos(
        doc: &Rope,
        pos: usize,
        offset_encoding: OffsetEncoding,
    ) -> lsp::Position {
        match offset_encoding {
            OffsetEncoding::Utf8 => {
                let line = doc.char_to_line(pos);
                let line_start = doc.line_to_char(line);
                let col = pos - line_start;

                lsp::Position::new(line as u32, col as u32)
            }
            OffsetEncoding::Utf16 => {
                let line = doc.char_to_line(pos);
                let line_start = doc.char_to_utf16_cu(doc.line_to_char(line));
                let col = doc.char_to_utf16_cu(pos) - line_start;

                lsp::Position::new(line as u32, col as u32)
            }
        }
    }

    /// Converts a range in the document to [`lsp::Range`].
    pub fn range_to_lsp_range(
        doc: &Rope,
        range: Range,
        offset_encoding: OffsetEncoding,
    ) -> lsp::Range {
        let start = pos_to_lsp_pos(doc, range.from(), offset_encoding);
        let end = pos_to_lsp_pos(doc, range.to(), offset_encoding);

        lsp::Range::new(start, end)
    }

    pub fn lsp_range_to_range(
        doc: &Rope,
        range: lsp::Range,
        offset_encoding: OffsetEncoding,
    ) -> Option<Range> {
        let start = lsp_pos_to_pos(doc, range.start, offset_encoding)?;
        let end = lsp_pos_to_pos(doc, range.end, offset_encoding)?;

        Some(Range::new(start, end))
    }

    pub fn generate_transaction_from_edits(
        doc: &Rope,
        edits: Vec<lsp::TextEdit>,
        offset_encoding: OffsetEncoding,
    ) -> Transaction {
        Transaction::change(
            doc,
            edits.into_iter().map(|edit| {
                // simplify "" into None for cleaner changesets
                let replacement = if !edit.new_text.is_empty() {
                    Some(edit.new_text.into())
                } else {
                    None
                };

                let start =
                    if let Some(start) = lsp_pos_to_pos(doc, edit.range.start, offset_encoding) {
                        start
                    } else {
                        return (0, 0, None);
                    };
                let end = if let Some(end) = lsp_pos_to_pos(doc, edit.range.end, offset_encoding) {
                    end
                } else {
                    return (0, 0, None);
                };
                (start, end, replacement)
            }),
        )
    }

    /// The result of asking the language server to format the document. This can be turned into a
    /// `Transaction`, but the advantage of not doing that straight away is that this one is
    /// `Send` and `Sync`.
    #[derive(Clone, Debug)]
    pub struct LspFormatting {
        pub doc: Rope,
        pub edits: Vec<lsp::TextEdit>,
        pub offset_encoding: OffsetEncoding,
    }

    impl From<LspFormatting> for Transaction {
        fn from(fmt: LspFormatting) -> Transaction {
            generate_transaction_from_edits(&fmt.doc, fmt.edits, fmt.offset_encoding)
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum MethodCall {
    WorkDoneProgressCreate(lsp::WorkDoneProgressCreateParams),
}

impl MethodCall {
    pub fn parse(method: &str, params: jsonrpc::Params) -> Option<MethodCall> {
        use lsp::request::Request;
        let request = match method {
            lsp::request::WorkDoneProgressCreate::METHOD => {
                let params: lsp::WorkDoneProgressCreateParams = params
                    .parse()
                    .expect("Failed to parse WorkDoneCreate params");
                Self::WorkDoneProgressCreate(params)
            }
            _ => {
                log::warn!("unhandled lsp request: {}", method);
                return None;
            }
        };
        Some(request)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum Notification {
    // we inject this notification to signal the LSP is ready
    Initialized,
    PublishDiagnostics(lsp::PublishDiagnosticsParams),
    ShowMessage(lsp::ShowMessageParams),
    LogMessage(lsp::LogMessageParams),
    ProgressMessage(lsp::ProgressParams),
}

impl Notification {
    pub fn parse(method: &str, params: jsonrpc::Params) -> Option<Notification> {
        use lsp::notification::Notification as _;

        let notification = match method {
            lsp::notification::Initialized::METHOD => Self::Initialized,
            lsp::notification::PublishDiagnostics::METHOD => {
                let params: lsp::PublishDiagnosticsParams = params
                    .parse()
                    .expect("Failed to parse PublishDiagnostics params");

                // TODO: need to loop over diagnostics and distinguish them by URI
                Self::PublishDiagnostics(params)
            }

            lsp::notification::ShowMessage::METHOD => {
                let params: lsp::ShowMessageParams = params.parse().ok()?;

                Self::ShowMessage(params)
            }
            lsp::notification::LogMessage::METHOD => {
                let params: lsp::LogMessageParams = params.parse().ok()?;

                Self::LogMessage(params)
            }
            lsp::notification::Progress::METHOD => {
                let params: lsp::ProgressParams = params.parse().ok()?;

                Self::ProgressMessage(params)
            }
            _ => {
                log::error!("unhandled LSP notification: {}", method);
                return None;
            }
        };

        Some(notification)
    }
}

#[derive(Debug)]
pub struct Registry {
    inner: HashMap<LanguageId, (usize, Arc<Client>)>,

    counter: AtomicUsize,
    pub incoming: SelectAll<UnboundedReceiverStream<(usize, Call)>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
            counter: AtomicUsize::new(0),
            incoming: SelectAll::new(),
        }
    }

    pub fn get_by_id(&self, id: usize) -> Option<&Client> {
        self.inner
            .values()
            .find(|(client_id, _)| client_id == &id)
            .map(|(_, client)| client.as_ref())
    }

    pub fn get(&mut self, language_config: &LanguageConfiguration) -> Result<Arc<Client>> {
        let config = match &language_config.language_server {
            Some(config) => config,
            None => return Err(Error::LspNotDefined),
        };

        match self.inner.entry(language_config.scope.clone()) {
            Entry::Occupied(entry) => Ok(entry.get().1.clone()),
            Entry::Vacant(entry) => {
                // initialize a new client
                let id = self.counter.fetch_add(1, Ordering::Relaxed);
                let (client, incoming, initialize_notify) = Client::start(
                    &config.command,
                    &config.args,
                    serde_json::from_str(language_config.config.as_deref().unwrap_or(""))
                        .map_err(|e| {
                            log::error!(
                                "LSP Config, {}, in `languages.toml` for `{}`",
                                e,
                                language_config.scope()
                            )
                        })
                        .ok(),
                    id,
                )?;
                self.incoming.push(UnboundedReceiverStream::new(incoming));
                let client = Arc::new(client);

                // Initialize the client asynchronously
                let _client = client.clone();
                tokio::spawn(async move {
                    use futures_util::TryFutureExt;
                    let value = _client
                        .capabilities
                        .get_or_try_init(|| {
                            _client
                                .initialize()
                                .map_ok(|response| response.capabilities)
                        })
                        .await;

                    value.expect("failed to initialize capabilities");

                    // next up, notify<initialized>
                    _client
                        .notify::<lsp::notification::Initialized>(lsp::InitializedParams {})
                        .await
                        .unwrap();

                    initialize_notify.notify_one();
                });

                entry.insert((id, client.clone()));
                Ok(client)
            }
        }
    }

    pub fn iter_clients(&self) -> impl Iterator<Item = &Arc<Client>> {
        self.inner.values().map(|(_, client)| client)
    }
}

#[derive(Debug)]
pub enum ProgressStatus {
    Created,
    Started(lsp::WorkDoneProgress),
}

impl ProgressStatus {
    pub fn progress(&self) -> Option<&lsp::WorkDoneProgress> {
        match &self {
            ProgressStatus::Created => None,
            ProgressStatus::Started(progress) => Some(progress),
        }
    }
}

#[derive(Default, Debug)]
/// Acts as a container for progress reported by language servers. Each server
/// has a unique id assigned at creation through [`Registry`]. This id is then used
/// to store the progress in this map.
pub struct LspProgressMap(HashMap<usize, HashMap<lsp::ProgressToken, ProgressStatus>>);

impl LspProgressMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a map of all tokens coresponding to the lanaguage server with `id`.
    pub fn progress_map(&self, id: usize) -> Option<&HashMap<lsp::ProgressToken, ProgressStatus>> {
        self.0.get(&id)
    }

    pub fn is_progressing(&self, id: usize) -> bool {
        self.0.get(&id).map(|it| !it.is_empty()).unwrap_or_default()
    }

    /// Returns last progress status for a given server with `id` and `token`.
    pub fn progress(&self, id: usize, token: &lsp::ProgressToken) -> Option<&ProgressStatus> {
        self.0.get(&id).and_then(|values| values.get(token))
    }

    /// Checks if progress `token` for server with `id` is created.
    pub fn is_created(&mut self, id: usize, token: &lsp::ProgressToken) -> bool {
        self.0
            .get(&id)
            .map(|values| values.get(token).is_some())
            .unwrap_or_default()
    }

    pub fn create(&mut self, id: usize, token: lsp::ProgressToken) {
        self.0
            .entry(id)
            .or_default()
            .insert(token, ProgressStatus::Created);
    }

    /// Ends the progress by removing the `token` from server with `id`, if removed returns the value.
    pub fn end_progress(
        &mut self,
        id: usize,
        token: &lsp::ProgressToken,
    ) -> Option<ProgressStatus> {
        self.0.get_mut(&id).and_then(|vals| vals.remove(token))
    }

    /// Updates the progess of `token` for server with `id` to `status`, returns the value replaced or `None`.
    pub fn update(
        &mut self,
        id: usize,
        token: lsp::ProgressToken,
        status: lsp::WorkDoneProgress,
    ) -> Option<ProgressStatus> {
        self.0
            .entry(id)
            .or_default()
            .insert(token, ProgressStatus::Started(status))
    }
}

#[cfg(test)]
mod tests {
    use super::{lsp, util::*, OffsetEncoding};
    use helix_core::Rope;

    #[test]
    fn converts_lsp_pos_to_pos() {
        macro_rules! test_case {
            ($doc:expr, ($x:expr, $y:expr) => $want:expr) => {
                let doc = Rope::from($doc);
                let pos = lsp::Position::new($x, $y);
                assert_eq!($want, lsp_pos_to_pos(&doc, pos, OffsetEncoding::Utf16));
                assert_eq!($want, lsp_pos_to_pos(&doc, pos, OffsetEncoding::Utf8))
            };
        }

        test_case!("", (0, 0) => Some(0));
        test_case!("", (0, 1) => None);
        test_case!("", (1, 0) => None);
        test_case!("\n\n", (0, 0) => Some(0));
        test_case!("\n\n", (1, 0) => Some(1));
        test_case!("\n\n", (1, 1) => Some(2));
        test_case!("\n\n", (2, 0) => Some(2));
        test_case!("\n\n", (3, 0) => None);
        test_case!("test\n\n\n\ncase", (4, 3) => Some(11));
        test_case!("test\n\n\n\ncase", (4, 4) => Some(12));
        test_case!("test\n\n\n\ncase", (4, 5) => None);
        test_case!("", (u32::MAX, u32::MAX) => None);
    }
}
