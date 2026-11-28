use crate::localization::{self, Appearance, Language, Preferences, is_english, tr};
use gpui_kit::{
    component::{
        ActiveTheme, Disableable, Icon,
        button::{Button, ButtonCustomVariant, ButtonVariants},
        input::{Input, InputState, Textarea, TextareaState},
        label::Label,
        popover::Popover,
        progress::Progress,
        text::TextView,
    },
    prelude::FluentBuilder,
    *,
};
use mesh_forum::DownloadProgress;
use mesh_forum::forum::{
    Bookmark, CacheLimits, LocalPostSource, LocalSearchHit, Node, PeerStatus, Post, PostHint,
    RevisionGroup, RevisionStatus, StorageUsage, group_revisions,
};
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSImage, NSWorkspace};
use objc2_foundation::NSData;
use std::{borrow::Cow, future::Future, io, path::PathBuf, sync::Arc};
use tokio::runtime::{Handle, Runtime};

const APP_ICON_PATH: &str = "mesh-forum-icon.png";
const APP_ICON: &[u8] = include_bytes!("../../assets/mesh-forum-icon.png");

struct ForumAssets;

impl AssetSource for ForumAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if path == APP_ICON_PATH {
            return Ok(Some(Cow::Borrowed(APP_ICON)));
        }
        assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut entries = assets::Assets.list(path)?;
        if APP_ICON_PATH.starts_with(path) {
            entries.push(APP_ICON_PATH.into());
        }
        Ok(entries)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Board,
    Mine,
    Bookmarks,
    Compose,
    Connections,
    Storage,
    Account,
}

actions!(
    mesh_forum,
    [
        FindPosts,
        DismissSearch,
        PreviousSearchHit,
        NextSearchHit,
        OpenSearchHit
    ]
);

enum Outcome {
    List(Vec<PostHint>),
    Account(Option<(String, [u8; 32])>),
    Board {
        page: Vec<PostHint>,
        history: Vec<PostHint>,
        offline: bool,
    },
    Open(Post, Option<Bookmark>, bool),
    Bookmarked(Option<Bookmark>, Vec<PostHint>),
    Published(Post, Vec<PostHint>, Vec<PeerStatus>),
    Peers(Vec<PeerStatus>),
    Message(&'static str),
    Withdrawn([u8; 32]),
    Storage(StorageUsage, Option<usize>),
}

#[derive(Clone)]
enum ReaderTransfer {
    Post {
        hint: PostHint,
        expected_local: bool,
    },
    Attachment {
        hint: PostHint,
        destination: Option<PathBuf>,
    },
}

struct TransferFeedback {
    operation: u64,
    request: ReaderTransfer,
    message: &'static str,
    failed: bool,
}

struct Desktop {
    node: Arc<Node>,
    focus: FocusHandle,
    preferences: Preferences,
    preferences_path: PathBuf,
    settings_open: bool,
    search_open: bool,
    search_input: Entity<InputState>,
    search_hits: Vec<LocalSearchHit>,
    search_generation: u64,
    search_query: Option<String>,
    search_selected: usize,
    search_scroll: ScrollHandle,
    article_scroll: ScrollHandle,
    searching: bool,
    test_locale: bool,
    account: Option<(String, [u8; 32])>,
    account_name: Entity<InputState>,
    password: Entity<InputState>,
    resume_composer: bool,
    composer_origin: View,
    recovery: Option<PathBuf>,
    account_transfer: bool,
    account_picker: bool,
    #[cfg(feature = "desktop-visual")]
    captured: std::collections::BTreeSet<&'static str>,
    runtime: Handle,
    view: View,
    entries: Vec<PostHint>,
    history: Vec<PostHint>,
    board_offline: bool,
    selected: Option<Post>,
    selected_local: bool,
    bookmark_mode: Option<Bookmark>,
    article_menu_open: bool,
    editing: Option<[u8; 32]>,
    preview: bool,
    reset_composer: bool,
    submit_attempted: bool,
    attachment: Option<PathBuf>,
    offset: u16,
    title: Entity<InputState>,
    body: Entity<TextareaState>,
    cache_posts: Entity<InputState>,
    cache_mib: Entity<InputState>,
    cache_days: Entity<InputState>,
    storage: Option<StorageUsage>,
    peers: Vec<PeerStatus>,
    busy: bool,
    opening: Option<[u8; 32]>,
    transfer: Option<TransferFeedback>,
    status: String,
    progress: Option<DownloadProgress>,
    operation: u64,
}

impl Desktop {
    fn new(
        node: Arc<Node>,
        runtime: Handle,
        preferences: Preferences,
        preferences_path: PathBuf,
        test_locale: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let title = cx.new(|cx| InputState::new(window, cx).placeholder(tr("帖子标题")));
        let body = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(12)
                .placeholder(tr("写下你的帖子"))
        });
        cx.observe(&title, |view: &mut Self, _, cx| {
            if view.submit_attempted {
                cx.notify();
            }
        })
        .detach();
        cx.observe(&body, |view: &mut Self, _, cx| {
            if view.submit_attempted {
                cx.notify();
            }
        })
        .detach();
        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr("搜索标题、作者或正文")));
        cx.observe(&search_input, |view: &mut Self, _, cx| {
            if view.search_open {
                view.refresh_search(cx);
            }
        })
        .detach();
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let mut view = Self {
            node,
            focus,
            preferences,
            preferences_path,
            settings_open: false,
            search_open: false,
            search_input,
            search_hits: Vec::new(),
            search_generation: 0,
            search_query: None,
            search_selected: 0,
            search_scroll: ScrollHandle::new(),
            article_scroll: ScrollHandle::new(),
            searching: false,
            test_locale,
            account: None,
            account_name: cx.new(|cx| InputState::new(window, cx).placeholder(tr("显示名称"))),
            password: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(tr("密码"))
                    .masked(true)
            }),
            resume_composer: false,
            composer_origin: View::Board,
            recovery: None,
            account_transfer: false,
            account_picker: false,
            #[cfg(feature = "desktop-visual")]
            captured: Default::default(),
            runtime,
            view: View::Board,
            entries: Vec::new(),
            history: Vec::new(),
            board_offline: false,
            selected: None,
            selected_local: false,
            bookmark_mode: None,
            article_menu_open: false,
            editing: None,
            preview: false,
            reset_composer: false,
            submit_attempted: false,
            attachment: None,
            offset: 0,
            title,
            body,
            cache_posts: cx.new(|cx| InputState::new(window, cx).placeholder(tr("篇数"))),
            cache_mib: cx.new(|cx| InputState::new(window, cx).placeholder(tr("容量（MiB）"))),
            cache_days: cx.new(|cx| InputState::new(window, cx).placeholder(tr("闲置天数"))),
            storage: None,
            peers: Vec::new(),
            busy: false,
            opening: None,
            transfer: None,
            status: String::new(),
            progress: None,
            operation: 0,
        };
        cx.observe_window_appearance(window, |view, window, cx| {
            if view.preferences.appearance == Appearance::System {
                apply_appearance(Appearance::System, Some(window), cx);
            }
        })
        .detach();
        view.refresh(cx);
        view.monitor(cx);
        view
    }

    fn start(
        &mut self,
        label: &str,
        future: impl Future<Output = io::Result<Outcome>> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.operation = self.operation.wrapping_add(1);
        self.progress = None;
        self.status = label.into();
        let operation = self.operation;
        let task = self.runtime.spawn(future);
        cx.spawn(async move |view, cx| {
            let result = task
                .await
                .map_err(io::Error::other)
                .and_then(|result| result);
            let _ = view.update(cx, |view, cx| {
                if view.operation != operation {
                    return;
                }
                view.opening = None;
                view.busy = false;
                view.progress = None;
                view.status.clear();
                if let Some(feedback) = view
                    .transfer
                    .as_mut()
                    .filter(|state| state.operation == operation)
                {
                    match &result {
                        Ok(Outcome::Message(message)) => feedback.message = message,
                        Ok(_) => view.transfer = None,
                        Err(error) => {
                            feedback.failed = true;
                            feedback.message = match (&feedback.request, error.kind()) {
                                (_, io::ErrorKind::InvalidData) => "下载校验失败，请重试。",
                                (_, io::ErrorKind::AlreadyExists) => {
                                    "保存位置已有同名文件，请另选位置。"
                                }
                                (
                                    ReaderTransfer::Post {
                                        expected_local: true,
                                        ..
                                    },
                                    io::ErrorKind::NotFound,
                                ) => "本机副本已清理。可从在线附近设备重新下载。",
                                (ReaderTransfer::Post { .. }, _) => {
                                    "未能读取帖子。请确认附近设备在线，然后重试。"
                                }
                                (ReaderTransfer::Attachment { .. }, _) => {
                                    "未能下载附件。请确认附近设备在线，然后重试。"
                                }
                            };
                        }
                    }
                }
                match result {
                    Ok(Outcome::Account(account)) => {
                        if view.recovery.is_some() && account.is_some() {
                            view.status = "账号已恢复".into();
                        }
                        view.account = account;
                        view.recovery = None;
                        view.account_transfer = false;
                        if view.account.is_some() && view.resume_composer {
                            view.view = View::Compose;
                            view.resume_composer = false;
                            view.status = "已登录，可以发布草稿".into();
                        }
                    }
                    Ok(Outcome::List(entries)) => view.entries = entries,
                    Ok(Outcome::Board {
                        page,
                        history,
                        offline,
                    }) => {
                        view.apply_board(page, history, offline);
                    }
                    Ok(Outcome::Open(post, mode, local)) => {
                        if view
                            .selected
                            .as_ref()
                            .is_none_or(|current| current.id != post.id)
                        {
                            view.article_scroll.set_offset(point(px(0.), px(0.)));
                        }
                        view.selected = Some(post);
                        view.selected_local = local;
                        view.bookmark_mode = mode;
                    }
                    Ok(Outcome::Bookmarked(mode, entries)) => {
                        view.bookmark_mode = mode;
                        if view.view == View::Bookmarks {
                            view.entries = entries;
                            if mode.is_none() {
                                view.selected = None;
                            }
                        }
                        view.status = match mode {
                            Some(Bookmark::Shared) => "已保存并共享",
                            Some(Bookmark::Local) => "已保存到本机",
                            None => "已取消书签",
                        }
                        .into();
                    }
                    Ok(Outcome::Published(post, entries, peers)) => {
                        view.status = "已发布到本机".into();
                        view.view = View::Mine;
                        view.peers = peers;
                        view.entries = entries;
                        view.article_scroll.set_offset(point(px(0.), px(0.)));
                        view.selected = Some(post);
                        view.selected_local = true;
                        view.bookmark_mode = None;
                        view.editing = None;
                        view.reset_composer = true;
                        view.preview = false;
                        view.attachment = None;
                    }
                    Ok(Outcome::Withdrawn(id)) => {
                        view.entries.retain(|item| item.thread_id() != id);
                        view.selected = None;
                        view.status = "已撤回".into();
                    }
                    Ok(Outcome::Storage(storage, message)) => {
                        view.storage = Some(storage);
                        if let Some(removed) = message {
                            view.status = if is_english() {
                                format!("Removed {removed} cached posts")
                            } else {
                                format!("已清理 {removed} 篇浏览缓存")
                            };
                        }
                    }
                    Ok(Outcome::Peers(peers)) => view.peers = peers,
                    Ok(Outcome::Message(message)) => view.status = message.into(),
                    Err(error) => {
                        if view
                            .transfer
                            .as_ref()
                            .is_none_or(|feedback| feedback.operation != operation)
                        {
                            view.status = localization::user_error(&error);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn start_download<F, Fut>(&mut self, label: &str, download: F, cx: &mut Context<Self>)
    where
        F: FnOnce(tokio::sync::watch::Sender<DownloadProgress>) -> Fut,
        Fut: Future<Output = io::Result<Outcome>> + Send + 'static,
    {
        if self.busy {
            return;
        }
        let (progress, mut updates) = tokio::sync::watch::channel(DownloadProgress::default());
        self.start(label, download(progress), cx);
        let operation = self.operation;
        cx.spawn(async move |view, cx| {
            while updates.changed().await.is_ok() {
                let progress = *updates.borrow_and_update();
                let is_current = view.update(cx, |view, cx| {
                    if !view.busy || view.operation != operation {
                        return false;
                    }
                    view.progress = Some(progress);
                    cx.notify();
                    true
                });
                if !matches!(is_current, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    fn apply_board(&mut self, page: Vec<PostHint>, history: Vec<PostHint>, offline: bool) {
        self.entries = page;
        self.history = history;
        self.board_offline = offline;
        if let Some(post) = &self.selected
            && !self
                .board_revisions()
                .iter()
                .any(|group| group.head.thread_id() == post.hint().thread_id())
        {
            self.selected = None;
        }
    }

    fn monitor(&self, cx: &mut Context<Self>) {
        let node = Arc::clone(&self.node);
        let runtime = self.runtime.clone();
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(3))
                    .await;
                let Ok(next) = view.update(cx, |view, _| {
                    (!view.busy).then_some((view.view, view.offset, view.operation))
                }) else {
                    break;
                };
                let Some((current, offset, operation)) = next else {
                    continue;
                };
                if !matches!(current, View::Board | View::Connections) {
                    continue;
                }
                let node = Arc::clone(&node);
                let Ok(Ok(outcome)) = runtime
                    .spawn(async move {
                        if current == View::Board {
                            load_board(&node, offset).await
                        } else {
                            Ok(Outcome::Peers(node.peer_status().await))
                        }
                    })
                    .await
                else {
                    continue;
                };
                if view
                    .update(cx, |view, cx| {
                        if view.busy
                            || view.view != current
                            || view.offset != offset
                            || view.operation != operation
                        {
                            return;
                        }
                        match outcome {
                            Outcome::Board {
                                page,
                                history,
                                offline,
                            } => view.apply_board(page, history, offline),
                            Outcome::Peers(peers) => view.peers = peers,
                            _ => return,
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let node = Arc::clone(&self.node);
        let offset = self.offset;
        let current = self.view;
        self.start(
            "正在加载帖子…",
            async move {
                let entries = match current {
                    View::Board => return load_board(&node, offset).await,
                    View::Mine | View::Bookmarks => {
                        node.local_posts(current == View::Bookmarks).await
                    }
                    _ => Vec::new(),
                };
                Ok(Outcome::List(entries))
            },
            cx,
        );
    }

    fn interrupt_read(&mut self) {
        if self.opening.take().is_some() {
            self.operation = self.operation.wrapping_add(1);
            self.busy = false;
            self.progress = None;
            self.status.clear();
            self.transfer = None;
        }
    }

    fn navigate(&mut self, view: View, cx: &mut Context<Self>) {
        self.interrupt_read();
        self.settings_open = false;
        self.search_open = false;
        self.transfer = None;
        self.article_menu_open = false;
        self.account_transfer = false;
        self.recovery = None;
        if view != View::Account {
            self.resume_composer = false;
        }
        self.view = view;
        self.selected = None;
        self.entries.clear();
        self.offset = 0;
        if view == View::Connections {
            self.refresh_peers(cx);
        }
        if view == View::Storage {
            let node = Arc::clone(&self.node);
            self.start(
                "正在读取存储用量…",
                async move { Ok(Outcome::Storage(node.storage().await?, None)) },
                cx,
            );
        }
        if matches!(view, View::Board | View::Mine | View::Bookmarks) {
            self.refresh(cx);
        }
        cx.notify();
    }

    fn open(&mut self, hint: PostHint, cx: &mut Context<Self>) {
        if self.opening == Some(hint.thread_id()) {
            return;
        }
        self.interrupt_read();
        if self.busy {
            return;
        }
        self.article_menu_open = false;
        self.opening = Some(hint.thread_id());
        self.run_reader_transfer(
            ReaderTransfer::Post {
                hint,
                expected_local: false,
            },
            cx,
        );
    }

    fn publish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let missing_title = self.title.read(cx).value().trim().is_empty();
        let missing_body = self.body.read(cx).value().trim().is_empty();
        if missing_title || missing_body {
            self.submit_attempted = true;
            self.preview = false;
            if missing_title {
                self.title.read(cx).focus_handle(cx).focus(window, cx);
            } else {
                self.body.read(cx).focus_handle(cx).focus(window, cx);
            }
            cx.notify();
            return;
        }
        self.submit_attempted = false;
        if self.account.is_none() {
            self.resume_composer = true;
            self.navigate(View::Account, cx);
            return;
        }
        let node = Arc::clone(&self.node);
        let title = self.title.read(cx).value().to_string();
        let body = self.body.read(cx).value().to_string();
        let editing = self.editing;
        let attachment = self.attachment.clone();
        self.start(
            "正在保存帖子…",
            async move {
                let post = match editing {
                    Some(id) => node.edit(id, &title, &body).await,
                    None => match attachment {
                        Some(path) => {
                            use std::io::Read;
                            let name = path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .ok_or_else(|| io::Error::other("附件文件名无效"))?;
                            let mut bytes = Vec::new();
                            std::fs::File::open(&path)?
                                .take(1024 * 1024 + 1)
                                .read_to_end(&mut bytes)?;
                            node.publish_attachment(&title, &body, name, &bytes).await
                        }
                        None => node.publish(&title, &body).await,
                    },
                }?;
                Ok(Outcome::Published(
                    post,
                    node.local_posts(false).await,
                    node.peer_status().await,
                ))
            },
            cx,
        );
    }

    fn bookmark(&mut self, id: [u8; 32], mode: Option<Bookmark>, cx: &mut Context<Self>) {
        let node = Arc::clone(&self.node);
        self.start(
            "正在更新书签…",
            async move {
                node.bookmark(id, mode).await?;
                Ok(Outcome::Bookmarked(mode, node.local_posts(true).await))
            },
            cx,
        );
    }

    fn show_account_transfer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.account_transfer = !self.account_transfer;
        self.recovery = None;
        self.status.clear();
        self.password
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    fn account_transfer_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let exporting = self.account.is_some();
        let title = if exporting {
            tr("备份账号")
        } else {
            tr("从备份恢复")
        };
        let file = self.recovery.as_ref().and_then(|path| path.file_name());
        let card = div()
            .p_5()
            .rounded_xl()
            .border_1()
            .border_color(surface(cx, 0xe8e2ef, 0x40364e))
            .bg(surface(cx, 0xf8f4fb, 0x282232))
            .flex()
            .flex_col()
            .gap_3();
        let content = if exporting {
            card.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(mesh_icon("hard-drive").text_color(accent(cx)))
                    .child(Label::new(tr("加密身份文件"))),
            )
            .child(Label::new(tr(
                "换设备后可用原账号继续发帖和编辑。恢复时需要备份生成时的账号密码。",
            )))
            .child(Label::new(tr("帖子、书签和附件留在原设备。")))
            .child(
                Button::new("choose-backup-destination")
                    .custom(brand_button(cx))
                    .bg(surface(cx, ACTION_FILL.0, ACTION_FILL.1))
                    .self_end()
                    .icon(mesh_icon("arrow-down"))
                    .label(tr("选择保存位置"))
                    .disabled(self.busy || self.account_picker)
                    .on_click(cx.listener(|this, _, _, cx| this.export_account(cx))),
            )
        } else {
            card.child(Label::new(tr("备份文件")))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .child(Label::new(
                                    file.map(|name| name.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| tr("尚未选择文件").into()),
                                )),
                        )
                        .child(
                            Button::new("choose-backup-file")
                                .icon(mesh_icon("folder-open"))
                                .label(if file.is_some() {
                                    tr("更换文件")
                                } else {
                                    tr("选择备份文件")
                                })
                                .disabled(self.busy || self.account_picker)
                                .on_click(cx.listener(|this, _, _, cx| this.choose_recovery(cx))),
                        ),
                )
                .when(file.is_some(), |card| {
                    card.child(Label::new(tr("备份生成时的账号密码")))
                        .child(Input::new(&self.password))
                        .child(
                            Button::new("restore-account")
                                .custom(brand_button(cx))
                                .bg(surface(cx, ACTION_FILL.0, ACTION_FILL.1))
                                .self_end()
                                .label(tr("恢复账号"))
                                .disabled(self.busy || self.account_picker)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.unlock(window, cx)),
                                ),
                        )
                })
        };
        div()
            .id("account-transfer")
            .size_full()
            .overflow_y_scroll()
            .max_w(px(540.))
            .mx_auto()
            .p_8()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                Button::new("back-to-account")
                    .ghost()
                    .self_start()
                    .icon(mesh_icon("arrow-left"))
                    .label(tr("返回账号"))
                    .disabled(self.busy || self.account_picker)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.show_account_transfer(window, cx)),
                    ),
            )
            .child(page_title(title))
            .child(content)
            .when(!self.status.is_empty(), |panel| {
                panel.child(
                    div()
                        .text_sm()
                        .child(Label::new(tr(&self.status).to_owned())),
                )
            })
    }

    fn unlock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let recovery = self.recovery.clone();
        let node = Arc::clone(&self.node);
        let name = self.account_name.read(cx).value().to_string();
        let password = zeroize::Zeroizing::new(self.password.read(cx).value().to_string());
        self.password
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.start(
            if recovery.is_some() {
                "正在恢复…"
            } else {
                "正在解锁账号…"
            },
            async move {
                if let Some(path) = recovery {
                    node.restore_account(&path, &password).await?;
                } else if node.has_account() {
                    node.login(&password).await?;
                } else {
                    node.create_account(&name, &password).await?;
                }
                Ok(Outcome::Account(
                    node.account_name().await.zip(node.active_author().await),
                ))
            },
            cx,
        );
    }

    fn test_account_backup_path() -> Option<PathBuf> {
        #[cfg(feature = "desktop-visual")]
        if let Some(path) = std::env::var_os("MESH_FORUM_TEST_BACKUP_PATH") {
            return Some(PathBuf::from(path));
        }
        None
    }

    fn choose_recovery(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.account_picker {
            return;
        }
        self.account_picker = true;
        cx.notify();
        let fixture = Self::test_account_backup_path();
        let selection = fixture.is_none().then(|| {
            cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some(tr("选择账号备份").into()),
            })
        });
        cx.spawn(async move |view, cx| {
            let outcome = match selection {
                Some(selection) => selection.await,
                None => Ok(Ok(fixture.map(|path| vec![path]))),
            };
            let _ = view.update(cx, |view, cx| {
                view.account_picker = false;
                cx.notify();
                if view.view != View::Account || !view.account_transfer || view.node.has_account() {
                    return;
                }
                match outcome {
                    Ok(Ok(Some(files))) => {
                        view.recovery = files.into_iter().next();
                        view.status.clear();
                    }
                    Ok(Ok(None)) => {}
                    _ => view.status = "无法打开文件选择器，请重试。".into(),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn export_account(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.account_picker {
            return;
        }
        self.account_picker = true;
        cx.notify();
        let directory = std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Documents"))
            .filter(|path| path.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        #[cfg(feature = "desktop-visual")]
        let directory = std::env::var_os("MESH_FORUM_TEST_BACKUP_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .unwrap_or(directory);
        let fixture = Self::test_account_backup_path();
        let destination = fixture
            .is_none()
            .then(|| cx.prompt_for_new_path(&directory, Some("Mesh Forum.mesh-account")));
        let node = Arc::clone(&self.node);
        cx.spawn(async move |view, cx| {
            let outcome = match destination {
                Some(destination) => destination.await,
                None => Ok(Ok(fixture)),
            };
            let _ = view.update(cx, |view, cx| {
                view.account_picker = false;
                cx.notify();
                if view.view != View::Account || !view.account_transfer || view.account.is_none() {
                    return;
                }
                match outcome {
                    Ok(Ok(Some(path))) => view.start(
                        "正在保存账号备份…",
                        async move {
                            node.export_account(&path).await?;
                            Ok(Outcome::Message("加密账号备份已保存"))
                        },
                        cx,
                    ),
                    Ok(Ok(None)) => {}
                    _ => view.status = "无法打开保存面板，请重试。".into(),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn logout(&mut self, cx: &mut Context<Self>) {
        let node = Arc::clone(&self.node);
        self.start(
            "正在退出账号…",
            async move {
                node.logout().await;
                Ok(Outcome::Account(None))
            },
            cx,
        );
    }

    fn compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.reset_composer {
            self.title
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.body
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.editing = None;
            self.attachment = None;
            self.preview = false;
            self.reset_composer = false;
        }
        self.enter_composer(window, cx);
    }

    fn enter_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view != View::Compose && !self.resume_composer {
            self.composer_origin = self.view;
        }
        self.navigate(View::Compose, cx);
        if !self.preview {
            self.title.read(cx).focus_handle(cx).focus(window, cx);
        }
    }

    fn begin_edit(&mut self, post: Post, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = Some(post.id);
        self.submit_attempted = false;
        self.reset_composer = false;
        self.preview = false;
        self.title
            .update(cx, |input, cx| input.set_value(post.title, window, cx));
        self.body
            .update(cx, |input, cx| input.set_value(post.body, window, cx));
        self.enter_composer(window, cx);
    }

    fn test_attachment_path(variable: &str) -> Option<PathBuf> {
        if cfg!(feature = "desktop-visual")
            && std::env::var_os("MESH_FORUM_TEST_BACKGROUND").is_some()
        {
            return std::env::var_os(variable).map(PathBuf::from);
        }
        None
    }

    fn pick_attachment(&mut self, cx: &mut Context<Self>) {
        let fixture = Self::test_attachment_path("MESH_FORUM_TEST_ATTACHMENT_PATH");
        let choice = fixture.is_none().then(|| {
            cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some(tr("选择附件").into()),
            })
        });
        cx.spawn(async move |view, cx| {
            let outcome = match choice {
                Some(choice) => choice.await,
                None => Ok(Ok(fixture.map(|path| vec![path]))),
            };
            if let Ok(Ok(Some(files))) = outcome {
                let _ = view.update(cx, |view, cx| {
                    view.attachment = files.into_iter().next();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn export_attachment(&mut self, hint: PostHint, cx: &mut Context<Self>) {
        let Some(file) = hint.attachment() else {
            return;
        };
        let fixture = Self::test_attachment_path("MESH_FORUM_TEST_ATTACHMENT_DESTINATION");
        let destination = fixture
            .is_none()
            .then(|| cx.prompt_for_new_path(&std::env::temp_dir(), Some(file.name())));
        cx.spawn(async move |view, cx| {
            let outcome = match destination {
                Some(destination) => destination.await,
                None => Ok(Ok(fixture)),
            };
            if let Ok(Ok(Some(path))) = outcome {
                let _ = view.update(cx, |view, cx| {
                    if view
                        .selected
                        .as_ref()
                        .is_some_and(|post| post.id == hint.id)
                    {
                        view.run_reader_transfer(
                            ReaderTransfer::Attachment {
                                hint,
                                destination: Some(path),
                            },
                            cx,
                        );
                    }
                });
            }
        })
        .detach();
    }

    fn run_reader_transfer(&mut self, request: ReaderTransfer, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let message = match &request {
            ReaderTransfer::Post { .. } => "正在下载并校验正文…",
            ReaderTransfer::Attachment {
                destination: Some(_),
                ..
            } => "正在下载附件…",
            ReaderTransfer::Attachment {
                destination: None, ..
            } => "正在保存附件…",
        };
        self.transfer = Some(TransferFeedback {
            operation: self.operation.wrapping_add(1),
            request: request.clone(),
            message,
            failed: false,
        });
        let node = Arc::clone(&self.node);
        self.start_download(
            message,
            move |progress| async move {
                match request {
                    ReaderTransfer::Post {
                        hint,
                        expected_local,
                    } => {
                        let post = match node.read(hint.id).await? {
                            Some(post) => post,
                            None => node
                                .download_with_progress(&hint, None, Some(&progress))
                                .await
                                .map_err(|error| {
                                    if expected_local && error.kind() != io::ErrorKind::InvalidData
                                    {
                                        io::Error::new(io::ErrorKind::NotFound, error)
                                    } else {
                                        error
                                    }
                                })?,
                        };
                        let mode = node.bookmark_mode(post.id).await;
                        let local = node.has_local_post(post.id).await;
                        Ok(Outcome::Open(post, mode, local))
                    }
                    ReaderTransfer::Attachment { hint, destination } => {
                        let bytes = node
                            .download_attachment_with_progress(
                                &hint,
                                destination.is_none(),
                                Some(&progress),
                            )
                            .await?;
                        if let Some(path) = destination {
                            use std::io::Write as _;
                            let mut file = tempfile::NamedTempFile::new_in(
                                path.parent().unwrap_or(std::path::Path::new(".")),
                            )?;
                            file.write_all(&bytes)?;
                            file.persist_noclobber(path)?;
                            Ok(Outcome::Message("附件已下载"))
                        } else {
                            Ok(Outcome::Message("附件已离线保存"))
                        }
                    }
                }
            },
            cx,
        );
    }

    fn transfer_feedback(&self, feedback: &TransferFeedback, cx: &mut Context<Self>) -> Div {
        let request = feedback.request.clone();
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .text_sm()
            .when(!feedback.failed, |row| {
                row.child(Label::new(tr(feedback.message)).text_color(secondary(cx)))
            })
            .when(feedback.failed, |row| {
                let ink = surface(cx, ERROR_TEXT.0, ERROR_TEXT.1);
                row.w_full()
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(surface(cx, 0xe7b5b8, 0x773d46))
                    .bg(surface(
                        cx,
                        TRANSFER_ERROR_SURFACE.0,
                        TRANSFER_ERROR_SURFACE.1,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(180.))
                            .flex()
                            .items_start()
                            .gap_2()
                            .child(
                                div()
                                    .pt_1()
                                    .flex_shrink_0()
                                    .child(mesh_icon("circle-alert").text_color(ink)),
                            )
                            .child(Label::new(tr(feedback.message)).text_color(ink)),
                    )
                    .child(
                        Button::new("retry-reader-transfer")
                            .outline()
                            .flex_shrink_0()
                            .icon(mesh_icon("refresh-cw").text_color(ink))
                            .label(tr("重试"))
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, _, cx| match &request {
                                ReaderTransfer::Post { hint, .. } => this.open(hint.clone(), cx),
                                ReaderTransfer::Attachment {
                                    hint,
                                    destination: Some(_),
                                } if feedback_message_requires_new_path(this.transfer.as_ref()) => {
                                    this.export_attachment(hint.clone(), cx)
                                }
                                _ => this.run_reader_transfer(request.clone(), cx),
                            })),
                    )
            })
    }
}

impl Desktop {
    fn refresh_peers(&mut self, cx: &mut Context<Self>) {
        let node = Arc::clone(&self.node);
        self.start(
            "正在检测节点…",
            async move { Ok(Outcome::Peers(node.peer_status().await)) },
            cx,
        );
    }

    fn toggle_uploads(&mut self, cx: &mut Context<Self>) {
        let node = Arc::clone(&self.node);
        let pause = !node.uploads_paused();
        self.start(
            "正在更新上传设置…",
            async move {
                node.set_uploads_paused(pause).await?;
                Ok(Outcome::Message(if pause {
                    "上传已暂停"
                } else {
                    "上传已恢复"
                }))
            },
            cx,
        );
    }
}

#[cfg(feature = "desktop-visual")]
impl Desktop {
    fn capture_frame(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(root) = std::env::var_os("MESH_FORUM_CAPTURE_DIR").map(PathBuf::from) else {
            return;
        };
        if self.busy {
            return;
        }
        let name = match self.view {
            _ if self.search_open && !self.search_hits.is_empty() => "search-results",
            _ if self.search_open => "search",
            _ if self.settings_open && is_english() => "settings-en",
            _ if self.settings_open => "settings-zh",
            _ if self
                .transfer
                .as_ref()
                .is_some_and(|feedback| feedback.failed) =>
            {
                "download-error"
            }
            View::Board if self.article_menu_open => "bookmark-menu",
            View::Board if self.bookmark_mode.is_some() => "saved",
            View::Board if self.selected.is_some() => "reading",
            View::Board if !self.entries.is_empty() => "board",
            View::Board => "empty",
            View::Mine => "published",
            View::Bookmarks => "bookmarks",
            View::Compose if self.preview => "preview",
            View::Compose => "composer",
            View::Connections => "nearby",
            View::Storage => "storage",
            View::Account if self.account_transfer && self.account.is_some() => "backup",
            View::Account if self.account_transfer => "restore",
            View::Account => "account",
        };
        if !self.captured.insert(name) {
            return;
        }
        let capture = move |window: &mut Window, _: &mut App| {
            let path = root.join(format!("{name}.png"));
            let result = std::fs::create_dir_all(&root)
                .map_err(io::Error::other)
                .and_then(|_| window.render_to_image().map_err(io::Error::other))
                .and_then(|image| image.save(&path).map_err(io::Error::other));
            if let Err(error) = result {
                eprintln!("Could not capture {name}: {error}");
            }
        };
        if std::env::var_os("MESH_FORUM_TEST_BACKGROUND").is_some() {
            window.defer(cx, capture);
        } else {
            window.on_next_frame(capture);
        }
    }
}

impl Render for Desktop {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(feature = "desktop-visual")]
        self.capture_frame(window, cx);
        let compact = window.viewport_size().width < px(850.);
        let writing = self.view == View::Compose && !self.settings_open && !self.search_open;
        let sidebar = div()
            .w(px(if compact { 148. } else { 184. }))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_5()
            .bg(surface(cx, 0xefebf5, 0x252131))
            .border_r_1()
            .border_color(surface(cx, 0xdce0e8, 0x333a48))
            .child(
                div()
                    .px_1()
                    .mb_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        img(ImageSource::Resource(Resource::Embedded(
                            APP_ICON_PATH.into(),
                        )))
                        .size(px(34.))
                        .flex_shrink_0(),
                    )
                    .child(
                        div()
                            .text_size(px(if compact { 11. } else { 16. }))
                            .whitespace_nowrap()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(Label::new("Mesh Forum")),
                    ),
            )
            .child(
                Button::new("compose")
                    .custom(brand_button(cx))
                    .bg(surface(cx, ACTION_FILL.0, ACTION_FILL.1))
                    .icon(mesh_icon("plus"))
                    .w_full()
                    .label(tr("写帖子"))
                    .disabled(self.busy && self.opening.is_none())
                    .on_click(cx.listener(|this, _, window, cx| this.compose(window, cx))),
            )
            .child(
                Button::new("find-posts")
                    .ghost()
                    .icon(mesh_icon("search"))
                    .label(tr("查找帖子"))
                    .tooltip("⌘F")
                    .w_full()
                    .on_click(cx.listener(|this, _, window, cx| this.show_search(window, cx))),
            )
            .child(div().h(px(10.)))
            .children(
                [
                    (View::Board, tr("版面"), "layout-dashboard"),
                    (View::Mine, tr("我的帖子"), "file-text"),
                    (View::Bookmarks, tr("书签"), "star"),
                ]
                .into_iter()
                .enumerate()
                .map(|(index, (view, title, icon))| self.nav_link(index, view, title, icon, cx)),
            )
            .child(div().flex_1())
            .when(self.node.uploads_paused(), |sidebar| {
                sidebar.child(
                    div()
                        .px_2()
                        .py_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(secondary(cx))
                        .child(
                            div()
                                .size(px(7.))
                                .rounded_full()
                                .bg(surface(cx, 0xb67b29, 0xf4b566)),
                        )
                        .child(tr("已暂停上传")),
                )
            })
            .child(
                div()
                    .mx_1()
                    .my_2()
                    .h(px(1.))
                    .bg(surface(cx, 0xd5dce9, 0x343d52)),
            )
            .children(
                [
                    (View::Connections, tr("节点"), "network"),
                    (View::Storage, tr("存储"), "hard-drive"),
                    (View::Account, tr("账号"), "circle-user"),
                ]
                .into_iter()
                .enumerate()
                .map(|(index, (view, title, icon))| {
                    self.nav_link(index + 3, view, title, icon, cx)
                }),
            )
            .child(
                Button::new("settings")
                    .ghost()
                    .icon(mesh_icon("settings"))
                    .label(tr("设置"))
                    .w_full()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.search_open = false;
                        this.settings_open = !this.settings_open;
                        cx.notify();
                    })),
            );

        let content = if self.search_open {
            self.search_panel(cx).into_any_element()
        } else if self.settings_open {
            self.settings_panel(cx).into_any_element()
        } else {
            match self.view {
                View::Compose => div()
                    .id("composer")
                    .size_full()
                    .max_w(px(820.))
                    .mx_auto()
                    .overflow_y_scroll()
                    .px_8()
                    .py_6()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .mb_2()
                            .child(page_title(if self.editing.is_some() {
                                tr("编辑帖子")
                            } else {
                                tr("写帖子")
                            }))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Button::new("composer-settings")
                                            .ghost()
                                            .icon(mesh_icon("settings"))
                                            .label(tr("设置"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.settings_open = true;
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("close-compose")
                                            .ghost()
                                            .icon(mesh_icon("close"))
                                            .label(tr("关闭"))
                                            .disabled(self.busy)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.navigate(this.composer_origin, cx)
                                            })),
                                    ),
                            ),
                    )
                    .when(self.account.is_none(), |pane| {
                        pane.child(
                            div()
                                .text_sm()
                                .text_color(secondary(cx))
                                .child(tr("发布时登录。")),
                        )
                    })
                    .child(Input::new(&self.title))
                    .when(
                        self.submit_attempted && self.title.read(cx).value().trim().is_empty(),
                        |pane| pane.child(field_error(tr("请填写帖子标题"), cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .self_start()
                            .p_1()
                            .rounded_lg()
                            .bg(surface(cx, 0xeceff5, 0x242a37))
                            .child(
                                Button::new("write-mode")
                                    .icon(mesh_icon("file-text"))
                                    .label(tr("撰写"))
                                    .ghost()
                                    .when(!self.preview, |button| {
                                        button.custom(selection_button(cx))
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.preview = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("preview-mode")
                                    .icon(mesh_icon("eye"))
                                    .label(tr("预览"))
                                    .ghost()
                                    .when(self.preview, |button| {
                                        button.custom(selection_button(cx))
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.preview = true;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(if self.preview {
                        div()
                            .min_h(px(240.))
                            .p_4()
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded_md()
                            .child(post_markdown(
                                "draft-preview",
                                self.body.read(cx).value().to_string(),
                            ))
                    } else {
                        div().child(Textarea::new(&self.body))
                    })
                    .when(
                        self.submit_attempted && self.body.read(cx).value().trim().is_empty(),
                        |pane| pane.child(field_error(tr("请填写帖子正文"), cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .pt_2()
                            .when(self.editing.is_none(), |row| {
                                let label = self
                                    .attachment
                                    .as_ref()
                                    .and_then(|path| path.file_name())
                                    .map(|name| name.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| tr("添加附件").into());
                                row.child(
                                    Button::new("pick")
                                        .icon(mesh_icon("plus"))
                                        .ghost()
                                        .label(label)
                                        .disabled(self.busy)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.pick_attachment(cx)),
                                        ),
                                )
                                .when(
                                    self.attachment.is_some(),
                                    |row| {
                                        row.child(
                                            Button::new("remove-attachment")
                                                .ghost()
                                                .icon(mesh_icon("close"))
                                                .accessibility_label(tr("移除附件"))
                                                .disabled(self.busy)
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.attachment = None;
                                                    cx.notify();
                                                })),
                                        )
                                    },
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                Button::new("publish")
                                    .custom(brand_button(cx))
                                    .bg(surface(cx, ACTION_FILL.0, ACTION_FILL.1))
                                    .px_5()
                                    .label(tr("发布"))
                                    .disabled(self.busy)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.publish(window, cx)),
                                    ),
                            ),
                    )
                    .into_any_element(),
                View::Connections => div()
                    .id("connections")
                    .size_full()
                    .overflow_y_scroll()
                    .max_w(px(760.))
                    .mx_auto()
                    .p_8()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(page_title(tr("附近节点")))
                    .when(self.peers.is_empty(), |pane| {
                        pane.child(Label::new(tr("正在自动发现同一局域网内的设备")))
                    })
                    .child(
                        div()
                            .mt_3()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                Button::new("toggle-uploads")
                                    .label(if self.node.uploads_paused() {
                                        tr("恢复上传")
                                    } else {
                                        tr("暂停上传")
                                    })
                                    .disabled(self.busy)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_uploads(cx)),
                                    ),
                            )
                            .child(
                                Button::new("refresh-peers")
                                    .label(tr("检测连接"))
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, _, cx| this.refresh_peers(cx))),
                            ),
                    )
                    .children(self.peers.iter().enumerate().map(|(index, peer)| {
                        let status = match peer.sharing {
                            Some(true) => tr("最近检测可达"),
                            Some(false) => tr("可达 · 已暂停上传"),
                            None => tr("未连通"),
                        };
                        div()
                            .id(("peer-status", index))
                            .p_4()
                            .rounded_lg()
                            .bg(surface(cx, 0xf0f3f8, 0x232936))
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(mesh_icon("globe").text_color(accent(cx)))
                            .child(Label::new(format!(
                                "{} {} · {status}",
                                tr("设备"),
                                index + 1
                            )))
                    }))
                    .into_any_element(),
                View::Account if self.account_transfer => {
                    self.account_transfer_panel(cx).into_any_element()
                }
                View::Account => {
                    let panel = div()
                        .id("account-panel")
                        .size_full()
                        .overflow_y_scroll()
                        .max_w(px(500.))
                        .mx_auto()
                        .p_8()
                        .flex()
                        .flex_col()
                        .gap_4();
                    if let Some((name, _)) = &self.account {
                        panel
                            .child(page_title(tr("账号")))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child(author_glyph(name, cx))
                                    .child(Label::new(name.clone())),
                            )
                            .child(
                                transfer_entry(
                                    "backup-account",
                                    tr("备份账号"),
                                    tr("本机发帖身份的加密副本"),
                                    cx,
                                )
                                .disabled(self.busy)
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.show_account_transfer(window, cx),
                                )),
                            )
                            .child(
                                Button::new("logout")
                                    .label(tr("退出登录"))
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, _, cx| this.logout(cx))),
                            )
                    } else {
                        let existing = self.node.has_account();
                        panel
                            .child(page_title(if existing {
                                tr("登录")
                            } else {
                                tr("创建账号")
                            }))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(secondary(cx))
                                    .child(tr("发布帖子需要登录。浏览和收藏无需账号。")),
                            )
                            .when(self.resume_composer, |panel| {
                                panel.child(
                                    Button::new("return-to-draft")
                                        .ghost()
                                        .icon(mesh_icon("arrow-left"))
                                        .label(tr("返回草稿"))
                                        .disabled(self.busy)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.enter_composer(window, cx)
                                        })),
                                )
                            })
                            .when(!existing, |panel| {
                                panel.child(Input::new(&self.account_name))
                            })
                            .child(Input::new(&self.password))
                            .child(
                                Button::new("unlock")
                                    .custom(brand_button(cx))
                                    .bg(surface(cx, ACTION_FILL.0, ACTION_FILL.1))
                                    .label(if existing {
                                        tr("登录")
                                    } else {
                                        tr("创建账号")
                                    })
                                    .disabled(self.busy)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.unlock(window, cx)),
                                    ),
                            )
                            .when(!existing, |panel| {
                                panel.child(
                                    transfer_entry(
                                        "recover-choice",
                                        tr("从备份恢复"),
                                        tr("在此设备继续使用原来的发帖身份"),
                                        cx,
                                    )
                                    .disabled(self.busy)
                                    .on_click(cx.listener(
                                        |this, _, window, cx| {
                                            this.show_account_transfer(window, cx)
                                        },
                                    )),
                                )
                            })
                    }
                    .into_any_element()
                }
                View::Storage => self.storage_panel(cx).into_any_element(),
                _ => self.reader(compact, cx).into_any_element(),
            }
        };

        div()
            .id("mesh-forum")
            .track_focus(&self.focus)
            .key_context("MeshForum")
            .on_action(cx.listener(|this, _: &FindPosts, window, cx| this.show_search(window, cx)))
            .size_full()
            .flex()
            .flex_col()
            .bg(surface(cx, CANVAS.0, CANVAS.1))
            .text_color(surface(cx, BODY_TEXT.0, BODY_TEXT.1))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .when(!writing, |layout| layout.child(sidebar))
                    .child(div().flex_1().min_w_0().min_h_0().child(content)),
            )
            .child(
                div()
                    .h(px(34.))
                    .flex_shrink_0()
                    .px_5()
                    .flex()
                    .items_center()
                    .gap_4()
                    .border_t_1()
                    .border_color(surface(cx, 0xe6eaf2, 0x333a49))
                    .bg(surface(cx, 0xf6f4f9, 0x201e2a))
                    .text_xs()
                    .text_color(secondary(cx))
                    .child(
                        div().flex_1().min_w_0().truncate().when(
                            !self.account_transfer
                                && self
                                    .transfer
                                    .as_ref()
                                    .is_none_or(|feedback| feedback.operation != self.operation),
                            |footer| footer.child(Label::new(tr(&self.status).to_owned())),
                        ),
                    )
                    .when_some(
                        self.progress.filter(|progress| progress.total_bytes != 0),
                        |footer, progress| {
                            let percent =
                                100. * progress.verified_bytes as f32 / progress.total_bytes as f32;
                            footer
                                .child(
                                    div().w(px(100.)).child(
                                        Progress::new("download-progress")
                                            .value(percent)
                                            .accessibility_label(tr("已校验下载进度")),
                                    ),
                                )
                                .child(format!("{percent:.0}%"))
                        },
                    ),
            )
    }
}

impl Desktop {
    fn show_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_open {
            self.search_query = None;
        }
        self.search_open = true;
        self.search_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        self.refresh_search(cx);
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_open = false;
        if self.view == View::Compose && !self.preview {
            self.title.read(cx).focus_handle(cx).focus(window, cx);
        } else {
            self.focus.focus(window, cx);
        }
        cx.notify();
    }

    fn refresh_search(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value().trim().to_owned();
        if self.search_query.as_deref() == Some(query.as_str()) {
            return;
        }
        self.search_query = Some(query.clone());
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        self.search_hits.clear();
        self.search_selected = 0;
        self.search_scroll.set_offset(point(px(0.), px(0.)));
        self.searching = !query.is_empty();
        cx.notify();
        if !self.searching {
            return;
        }
        let node = Arc::clone(&self.node);
        let runtime = self.runtime.clone();
        cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(120))
                .await;
            if !view
                .read_with(cx, |view, _| {
                    view.search_open && view.search_generation == generation
                })
                .unwrap_or(false)
            {
                return;
            }
            let hits = runtime
                .spawn(async move { node.search_local_posts(&query).await })
                .await
                .unwrap_or_default();
            let _ = view.update(cx, |view, cx| {
                if view.search_open && view.search_generation == generation {
                    view.search_hits = hits;
                    view.searching = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn open_search_hit(
        &mut self,
        hit: LocalSearchHit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy && self.opening.is_none() {
            return;
        }
        self.interrupt_read();
        self.search_open = false;
        self.focus.focus(window, cx);
        self.settings_open = false;
        self.view = match hit.source {
            LocalPostSource::Owned => View::Mine,
            LocalPostSource::Bookmarked => View::Bookmarks,
            LocalPostSource::Cached => View::Board,
        };
        self.offset = 0;
        self.selected = None;
        self.bookmark_mode = None;
        self.entries = vec![hit.hint.clone()];
        self.board_offline = false;
        self.article_menu_open = false;
        self.opening = Some(hit.hint.thread_id());
        self.run_reader_transfer(
            ReaderTransfer::Post {
                hint: hit.hint,
                expected_local: true,
            },
            cx,
        );
    }

    fn search_panel(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let query = self.search_input.read(cx).value().trim().to_owned();
        div()
            .id("local-search")
            .key_context("MeshForumSearch")
            .on_action(cx.listener(|this, _: &DismissSearch, window, cx| {
                this.close_search(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousSearchHit, _, cx| {
                this.search_selected = this.search_selected.saturating_sub(1);
                this.search_scroll.scroll_to_item(this.search_selected + 3);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextSearchHit, _, cx| {
                this.search_selected =
                    (this.search_selected + 1).min(this.search_hits.len().saturating_sub(1));
                this.search_scroll.scroll_to_item(this.search_selected + 3);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenSearchHit, window, cx| {
                if let Some(hit) = this.search_hits.get(this.search_selected).cloned() {
                    this.open_search_hit(hit, window, cx);
                }
            }))
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.search_scroll)
            .max_w(px(740.))
            .mx_auto()
            .p_8()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(page_title(tr("查找帖子")))
                    .child(
                        Button::new("close-search")
                            .ghost()
                            .label(tr("关闭搜索"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.search_open = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(Label::new(tr("只搜索本机的帖子与缓存。")).text_color(secondary(cx)))
            .child(Input::new(&self.search_input))
            .when(self.search_hits.is_empty(), |panel| {
                panel.child(Label::new(tr(if query.is_empty() {
                    "输入关键词开始搜索。"
                } else if self.searching {
                    "正在搜索…"
                } else {
                    "没有找到匹配的本机帖子。"
                })))
            })
            .children(self.search_hits.iter().enumerate().map(|(index, hit)| {
                let title = hit.hint.title.clone();
                let source = tr(match hit.source {
                    LocalPostSource::Owned => "我的帖子",
                    LocalPostSource::Bookmarked => "书签",
                    LocalPostSource::Cached => "浏览缓存",
                });
                let row = div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap_1()
                    .child(
                        Label::new(title.clone())
                            .id(("search-title", index))
                            .highlights(query.clone()),
                    )
                    .child(
                        Label::new(source)
                            .id(("search-source", index))
                            .text_color(secondary(cx)),
                    )
                    .when(!hit.snippet.is_empty(), |row| {
                        row.child(
                            div().w_full().truncate().text_sm().child(
                                Label::new(hit.snippet.clone())
                                    .id(("search-snippet", index))
                                    .highlights(query.clone()),
                            ),
                        )
                    });
                let hit = hit.clone();
                Button::new(("search-result", index))
                    .ghost()
                    .w_full()
                    .h(px(72.))
                    .when(index == self.search_selected, |button| {
                        button.custom(selection_button(cx))
                    })
                    .accessibility_label(format!("{title} · {source}"))
                    .child(row)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_search_hit(hit.clone(), window, cx)
                    }))
            }))
    }

    fn settings_panel(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let language = [
            (Language::System, tr("跟随系统")),
            (Language::Chinese, "简体中文"),
            (Language::English, "English"),
        ];
        let appearance = [
            (Appearance::System, tr("跟随系统")),
            (Appearance::Light, tr("浅色")),
            (Appearance::Dark, tr("深色")),
        ];
        div()
            .id("settings-panel")
            .size_full()
            .overflow_y_scroll()
            .max_w(px(720.))
            .mx_auto()
            .p_8()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                Button::new("close-settings")
                    .ghost()
                    .self_start()
                    .icon(mesh_icon("arrow-left"))
                    .label(tr("完成"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.settings_open = false;
                        cx.notify();
                    })),
            )
            .child(page_title(tr("外观与语言")))
            .child(Label::new(tr("界面语言")))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(
                        language
                            .into_iter()
                            .enumerate()
                            .map(|(index, (value, label))| {
                                Button::new(("language", index))
                                    .label(label)
                                    .toggled(self.preferences.language == value)
                                    .when(value == Language::System, |button| {
                                        button.accessibility_label(format!(
                                            "{} · {label}",
                                            tr("界面语言")
                                        ))
                                    })
                                    .when(self.preferences.language == value, |button| {
                                        button
                                            .custom(brand_button(cx))
                                            .bg(surface(cx, ACTION_FILL.0, ACTION_FILL.1))
                                            .icon(mesh_icon("check"))
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.set_preferences(
                                            Preferences {
                                                language: value,
                                                ..this.preferences
                                            },
                                            window,
                                            cx,
                                        );
                                    }))
                            }),
                    ),
            )
            .child(Label::new(tr("主题")))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(
                        appearance
                            .into_iter()
                            .enumerate()
                            .map(|(index, (value, label))| {
                                Button::new(("appearance", index))
                                    .label(label)
                                    .toggled(self.preferences.appearance == value)
                                    .when(value == Appearance::System, |button| {
                                        button.accessibility_label(format!(
                                            "{} · {label}",
                                            tr("主题")
                                        ))
                                    })
                                    .when(self.preferences.appearance == value, |button| {
                                        button
                                            .custom(brand_button(cx))
                                            .bg(surface(cx, ACTION_FILL.0, ACTION_FILL.1))
                                            .icon(mesh_icon("check"))
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.set_preferences(
                                            Preferences {
                                                appearance: value,
                                                ..this.preferences
                                            },
                                            window,
                                            cx,
                                        );
                                    }))
                            }),
                    ),
            )
            .child(div().mt_4().child(Label::new(tr("数据与连接"))))
            .child(
                div().flex().flex_wrap().gap_2().children(
                    [
                        (View::Storage, tr("存储与缓存")),
                        (View::Connections, tr("附近节点")),
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(index, (view, label))| {
                        Button::new(("related-settings", index))
                            .label(label)
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, _, cx| this.navigate(view, cx)))
                    }),
                ),
            )
    }

    fn set_preferences(
        &mut self,
        preferences: Preferences,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let old = self.preferences;
        self.preferences = preferences;
        if self.preferences.save(&self.preferences_path).is_err() {
            self.status = "无法保存偏好设置。".into();
        }
        if preferences.language != old.language {
            localization::activate(preferences.language, self.test_locale);
            for (field, label) in [
                (&self.title, "帖子标题"),
                (&self.search_input, "搜索标题、作者或正文"),
                (&self.account_name, "显示名称"),
                (&self.password, "密码"),
                (&self.cache_posts, "篇数"),
                (&self.cache_mib, "容量（MiB）"),
                (&self.cache_days, "闲置天数"),
            ] {
                field.update(cx, |field, cx| field.set_placeholder(tr(label), window, cx));
            }
            self.body.update(cx, |field, cx| {
                field.set_placeholder(tr("写下你的帖子"), window, cx)
            });
        }
        if preferences.appearance != old.appearance {
            apply_appearance(preferences.appearance, Some(window), cx);
        }
        cx.notify();
    }

    fn board_revisions(&self) -> Vec<RevisionGroup> {
        let visible: std::collections::BTreeSet<_> =
            self.entries.iter().map(PostHint::thread_id).collect();
        group_revisions(
            self.entries.iter().chain(
                self.history
                    .iter()
                    .filter(|hint| visible.contains(&hint.thread_id())),
            ),
        )
    }

    fn storage_panel(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let panel = div()
            .id("storage-panel")
            .size_full()
            .overflow_y_scroll()
            .max_w(px(760.))
            .mx_auto()
            .p_8()
            .flex()
            .flex_col()
            .gap_3()
            .child(page_title(tr("存储与缓存")));
        let Some(usage) = &self.storage else {
            return panel;
        };
        let mib = |bytes: u64| format!("{:.2} MiB", bytes as f64 / 1_048_576.);
        let usage_rows = [
            (
                "file-text",
                tr("我的帖子"),
                format!(
                    "{} {} · {}",
                    usage.owned_posts,
                    tr("篇"),
                    mib(usage.owned_bytes)
                ),
            ),
            (
                "star",
                tr("离线书签"),
                format!(
                    "{} {} · {}",
                    usage.bookmarked_posts,
                    tr("篇"),
                    mib(usage.bookmarked_bytes)
                ),
            ),
            (
                "hard-drive",
                tr("浏览缓存"),
                format!(
                    "{} {} · {}",
                    usage.cached_posts,
                    tr("篇"),
                    mib(usage.cached_bytes)
                ),
            ),
            (
                "arrow-down",
                tr("下载暂存"),
                usage
                    .partial_bytes
                    .map(mib)
                    .unwrap_or_else(|| tr("下载中").into()),
            ),
        ];
        let limits = [
            (
                format!("{} · {}", tr("最多缓存篇数"), usage.limits.posts),
                &self.cache_posts,
            ),
            (
                format!("{} · {}", tr("缓存容量"), mib(usage.limits.bytes)),
                &self.cache_mib,
            ),
            (
                format!(
                    "{} · {:.2}",
                    tr("闲置回收天数"),
                    usage.limits.idle_seconds as f64 / 86400.
                ),
                &self.cache_days,
            ),
        ];
        panel
            .child(
                div()
                    .p_4()
                    .rounded_lg()
                    .bg(surface(cx, 0xf1f4f9, 0x232a38))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .children(usage_rows.into_iter().enumerate().map(
                        |(index, (icon, title, value))| {
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(mesh_icon(icon).text_color(accent(cx)))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_sm()
                                        .child(Label::new(title).id(("usage-title", index))),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(secondary(cx))
                                        .child(Label::new(value).id(("usage-value", index))),
                                )
                        },
                    )),
            )
            .child(
                div()
                    .pt_3()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(Label::new(tr("自动清理"))),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(secondary(cx))
                    .child(Label::new(tr("这里的设置只影响浏览缓存。"))),
            )
            .children(
                limits
                    .into_iter()
                    .enumerate()
                    .map(|(index, (label, field))| {
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .text_sm()
                                    .child(Label::new(label).id(("limit-label", index))),
                            )
                            .child(div().max_w(px(144.)).w_full().child(Input::new(field)))
                    }),
            )
            .child(
                div()
                    .pt_3()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        Button::new("save-cache")
                            .custom(brand_button(cx))
                            .bg(surface(cx, ACTION_FILL.0, ACTION_FILL.1))
                            .label(tr("保存设置"))
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| this.change_cache(false, cx))),
                    )
                    .child(
                        Button::new("clear-cache")
                            .ghost()
                            .icon(mesh_icon("delete"))
                            .label(format!("{} · {}", tr("清理缓存"), mib(usage.cached_bytes)))
                            .disabled(self.busy || usage.cached_posts == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.change_cache(true, cx))),
                    ),
            )
    }

    fn change_cache(&mut self, clear: bool, cx: &mut Context<Self>) {
        let Some(usage) = &self.storage else {
            return;
        };
        let node = Arc::clone(&self.node);
        let parse = |field: &Entity<InputState>, default, scale| {
            let value = field.read(cx).value();
            let value = value.trim();
            if value.is_empty() {
                return Ok(default);
            }
            value
                .parse::<u64>()
                .ok()
                .and_then(|number| number.checked_mul(scale))
                .ok_or_else(|| io::Error::other(tr("请输入有效的非负整数")))
        };
        let limits = (|| {
            Ok::<_, io::Error>(CacheLimits {
                posts: usize::try_from(parse(&self.cache_posts, usage.limits.posts as u64, 1)?)
                    .map_err(io::Error::other)?,
                bytes: parse(&self.cache_mib, usage.limits.bytes, 1_048_576)?,
                idle_seconds: parse(&self.cache_days, usage.limits.idle_seconds, 86_400)?,
            })
        })();
        self.start(
            "正在更新缓存…",
            async move {
                let removed = if clear {
                    node.clear_cache().await?
                } else {
                    node.configure_cache(limits?).await?
                };
                Ok(Outcome::Storage(node.storage().await?, Some(removed)))
            },
            cx,
        );
    }

    fn nav_link(
        &self,
        id: usize,
        view: View,
        title: &'static str,
        icon: &'static str,
        cx: &mut Context<Self>,
    ) -> Button {
        let active = self.view == view;
        let button = Button::new(("nav", id))
            .w_full()
            .h(px(36.))
            .accessibility_label(title)
            .disabled(self.busy && self.opening.is_none())
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(mesh_icon(icon).text_color(if active {
                        accent(cx)
                    } else {
                        cx.theme().muted_foreground
                    }))
                    .child(title),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.navigate(view, cx)));
        if active {
            button
                .custom(selection_button(cx))
                .bg(surface(cx, 0xe8e0f7, 0x3a3053))
        } else {
            button.ghost()
        }
    }

    fn article_actions(&self, post: &Post, owner: bool, cx: &mut Context<Self>) -> Div {
        let id = post.id;
        let thread = post.hint().thread_id();
        let mode = self.bookmark_mode;
        let saved = mode.is_some();
        let open = self.article_menu_open;
        let view = cx.weak_entity();
        let menu_target = view.clone();
        let trigger = if owner {
            article_button("post-menu", tr("更多操作"), "ellipsis", self.busy, cx)
        } else {
            Button::new("bookmark-menu")
                .ghost()
                .size_8()
                .rounded_lg()
                .disabled(self.busy)
                .accessibility_label(tr("帖子书签"))
                .tooltip(match mode {
                    Some(Bookmark::Shared) => tr("已保存并共享"),
                    Some(Bookmark::Local) => tr("已保存到本机"),
                    None => tr("保存到书签"),
                })
                .child(
                    mesh_icon(if saved { "star-fill" } else { "star" })
                        .size(px(17.))
                        .text_color(if saved {
                            surface(cx, 0x996009, 0xffcc74).into()
                        } else {
                            accent(cx)
                        })
                        .with_animation(
                            ("bookmark-state", saved as usize),
                            Animation::new(std::time::Duration::from_millis(150)),
                            |icon, t| icon.opacity(0.62 + 0.38 * t),
                        ),
                )
        };
        let menu = Popover::new("article-popover")
            .anchor(Anchor::TopRight)
            .mouse_button(MouseButton::Right)
            .open(open)
            .trigger(trigger.on_click(cx.listener(|this, _, _, cx| {
                this.article_menu_open = !this.article_menu_open;
                cx.notify();
            })))
            .on_open_change(move |open, _, cx| {
                let _ = view.update(cx, |view, cx| {
                    view.article_menu_open = *open;
                    cx.notify();
                });
            })
            .bg(surface(cx, 0xfdfaff, 0x292334))
            .content(move |_, _, _| {
                let mut panel = div().min_w(px(174.)).flex().flex_col().gap_1();
                if owner {
                    let target = menu_target.clone();
                    return panel.child(
                        article_menu_row("withdraw", tr("撤回帖子"), "delete", false).on_click(
                            move |_, window, cx| {
                                let _ = target.update(cx, |view, cx| {
                                    view.article_menu_open = false;
                                    view.confirm_withdraw(id, thread, window, cx);
                                    cx.notify();
                                });
                            },
                        ),
                    );
                }
                let actions = [
                    (
                        "share",
                        tr("保存并共享"),
                        "network",
                        Some(Bookmark::Shared),
                        matches!(mode, Some(Bookmark::Shared)),
                    ),
                    (
                        "local",
                        tr("仅本地保存"),
                        "hard-drive",
                        Some(Bookmark::Local),
                        matches!(mode, Some(Bookmark::Local)),
                    ),
                    ("remove", tr("取消书签"), "star-off", None, false),
                ];
                for (key, title, icon, next, selected) in
                    actions.into_iter().take(if saved { 3 } else { 2 })
                {
                    let target = menu_target.clone();
                    panel = panel.child(article_menu_row(key, title, icon, selected).on_click(
                        move |_, _, cx| {
                            let _ = target.update(cx, |view, cx| {
                                view.article_menu_open = false;
                                view.bookmark(id, next, cx);
                                cx.notify();
                            });
                        },
                    ));
                }
                panel
            });
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .p_1()
            .rounded_xl()
            .border_1()
            .border_color(surface(cx, 0xe8e2ef, 0x40364e))
            .bg(surface(cx, 0xf8f4fb, 0x282232))
            .when(owner, |toolbar| {
                let editable = post.clone();
                toolbar.child(
                    article_button("edit", tr("编辑"), "file-text", self.busy, cx).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.begin_edit(editable.clone(), window, cx)
                        }),
                    ),
                )
            })
            .child(menu)
    }

    fn confirm_withdraw(
        &mut self,
        id: [u8; 32],
        thread: [u8; 32],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let choice = window.prompt(
            PromptLevel::Warning,
            tr("撤回这篇帖子？"),
            Some(tr("收到撤回通知的设备会移除这篇帖子。")),
            &[tr("取消"), tr("撤回")],
            cx,
        );
        cx.spawn(async move |view, cx| {
            if choice.await == Ok(1) {
                let _ = view.update(cx, |view, cx| {
                    let node = Arc::clone(&view.node);
                    view.start(
                        "正在撤回…",
                        async move {
                            node.withdraw(id).await?;
                            Ok(Outcome::Withdrawn(thread))
                        },
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    fn reader(&self, compact: bool, cx: &mut Context<Self>) -> Div {
        let mut list = div()
            .when(compact, |list| list.w_full())
            .when(!compact, |list| list.w(px(302.)).flex_shrink_0())
            .h_full()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(surface(cx, 0xdce0e8, 0x333a48))
            .bg(surface(cx, 0xf6f4f9, 0x201e2a));
        let refresh = Button::new("refresh")
            .ghost()
            .icon(mesh_icon("refresh-cw"))
            .accessibility_label(tr("刷新"))
            .tooltip(tr("刷新"))
            .disabled(self.busy)
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)));
        let pagination = div()
            .px_4()
            .py_2()
            .flex()
            .items_center()
            .justify_between()
            .child(
                Button::new("prev")
                    .ghost()
                    .icon(mesh_icon("chevron-left"))
                    .accessibility_label(tr("上一页"))
                    .disabled(self.busy || self.offset == 0)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.offset = this.offset.saturating_sub(30);
                        this.refresh(cx);
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(secondary(cx))
                    .child(if is_english() {
                        format!("Page {}", self.offset / 30 + 1)
                    } else {
                        format!("第 {} 页", self.offset / 30 + 1)
                    }),
            )
            .child(
                Button::new("next")
                    .ghost()
                    .icon(mesh_icon("chevron-right"))
                    .accessibility_label(tr("下一页"))
                    .disabled(self.busy || self.offset >= 180 || self.entries.len() < 30)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.offset += 30;
                        this.refresh(cx);
                    })),
            );
        let revisions = if self.view == View::Bookmarks {
            self.entries
                .iter()
                .cloned()
                .map(|head| RevisionGroup {
                    head,
                    status: RevisionStatus::Current,
                })
                .collect()
        } else {
            if self.view == View::Board {
                self.board_revisions()
            } else {
                group_revisions(&self.entries)
            }
        };
        let revision_note = |status| match status {
            RevisionStatus::Current => None,
            RevisionStatus::MissingPrevious => Some(tr("缺少中间版本")),
            RevisionStatus::Conflict => Some(tr("版本有冲突")),
        };
        let heading = match self.view {
            View::Board => tr("附近帖子"),
            View::Bookmarks => tr("书签"),
            _ => tr("我的帖子"),
        };
        list = list
            .child(
                div()
                    .px_5()
                    .pt_5()
                    .pb_3()
                    .flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(23.))
                            .line_height(px(31.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(heading),
                    )
                    .child(refresh),
            )
            .when(self.view == View::Board && self.board_offline, |list| {
                list.child(
                    div()
                        .mx_4()
                        .mb_2()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(surface(cx, 0xe9eef8, 0x272e3e))
                        .text_xs()
                        .text_color(secondary(cx))
                        .child(tr("离线索引 · 来源待确认")),
                )
            })
            .child(
                div()
                    .id("post-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .pb_3()
                    .children(revisions.iter().map(|group| {
                        let hint = group.head.clone();
                        let label: SharedString = match (
                            revision_note(group.status),
                            self.view == View::Board
                                && (self.board_offline
                                    || !self.entries.iter().any(|entry| entry.id == hint.id)),
                        ) {
                            (Some(note), _) => format!("{} · {note}", hint.title).into(),
                            (None, true) => format!("{} · {}", hint.title, tr("来源待确认")).into(),
                            (None, false) => hint.title.clone().into(),
                        };
                        let selected = self.opening.map_or_else(
                            || {
                                self.selected
                                    .as_ref()
                                    .is_some_and(|post| post.hint().thread_id() == hint.thread_id())
                            },
                            |thread| thread == hint.thread_id(),
                        );
                        let id: SharedString = hint
                            .id
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>()
                            .into();
                        let subtitle = hint.author_name().unwrap_or(tr("旧版作者")).to_owned();
                        Button::new((ElementId::from("post"), id))
                            .w_full()
                            .h(px(64.))
                            .when(selected, |button| {
                                button
                                    .custom(selection_button(cx))
                                    .bg(surface(cx, 0xe8e0f7, 0x3a3053))
                            })
                            .when(!selected, |button| button.ghost())
                            .accessibility_label(label.clone())
                            .flex_shrink_0()
                            .child(
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .px_1()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .child(author_glyph(&subtitle, cx))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .items_start()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .w_full()
                                                    .truncate()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .child(hint.title.clone()),
                                            )
                                            .child(
                                                div()
                                                    .w_full()
                                                    .truncate()
                                                    .text_xs()
                                                    .text_color(secondary(cx))
                                                    .child(match revision_note(group.status) {
                                                        Some(note) => {
                                                            format!("{subtitle} · {note}")
                                                        }
                                                        None => subtitle,
                                                    }),
                                            ),
                                    ),
                            )
                            .disabled(self.busy && self.opening.is_none())
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.open(hint.clone(), cx)),
                            )
                    }))
                    .when(revisions.is_empty(), |list| {
                        list.child(
                            div()
                                .px_3()
                                .py_6()
                                .text_sm()
                                .text_color(secondary(cx))
                                .child(Label::new(if self.busy {
                                    tr("正在加载…")
                                } else {
                                    match self.view {
                                        View::Bookmarks => tr("收藏的帖子会出现在这里。"),
                                        View::Mine => tr("你发布的帖子会出现在这里。"),
                                        _ => tr("暂无附近帖子。发现设备后会自动更新。"),
                                    }
                                })),
                        )
                    }),
            )
            .when(
                self.view == View::Board && (self.offset > 0 || self.entries.len() >= 30),
                |list| list.child(pagination),
            );
        let detail = div()
            .id((
                ElementId::from("post-detail"),
                self.selected.as_ref().map_or_else(
                    || SharedString::from("empty"),
                    |post| {
                        post.id
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>()
                            .into()
                    },
                ),
            ))
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.article_scroll)
            .px(px(if compact { 24. } else { 38. }));
        let mut article = div().max_w(px(660.)).mx_auto().py_8();
        let post_feedback = self
            .transfer
            .as_ref()
            .filter(|state| matches!(state.request, ReaderTransfer::Post { .. }));
        if let Some(feedback) = post_feedback {
            if compact && self.selected.is_none() {
                list = list.child(
                    div()
                        .px_4()
                        .pb_3()
                        .child(self.transfer_feedback(feedback, cx)),
                );
            } else {
                article = article.child(self.transfer_feedback(feedback, cx));
            }
        }
        if let Some(post) = &self.selected {
            if compact {
                article = article.child(
                    div().mb_5().flex().child(
                        Button::new("back-to-list")
                            .icon(mesh_icon("arrow-left"))
                            .label(tr("返回列表"))
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.interrupt_read();
                                this.selected = None;
                                this.article_menu_open = false;
                                this.bookmark_mode = None;
                                cx.notify();
                            })),
                    ),
                );
            }
            let display_author = post.hint().author_name().unwrap_or(tr("旧版作者"));
            let availability = if self.bookmark_mode.is_some()
                || self
                    .account
                    .as_ref()
                    .is_some_and(|(_, key)| *key == post.hint().author())
            {
                tr("可离线阅读")
            } else if self.selected_local {
                tr("已缓存在本机")
            } else {
                tr("正在查看临时副本")
            };

            let owner = self
                .account
                .as_ref()
                .is_some_and(|(_, key)| *key == post.hint().author());
            article = article
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .mb_5()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .min_w_0()
                                .child(author_glyph(display_author, cx))
                                .child(
                                    div()
                                        .truncate()
                                        .text_sm()
                                        .text_color(secondary(cx))
                                        .child(display_author.to_owned()),
                                ),
                        )
                        .child(self.article_actions(post, owner, cx)),
                )
                .child(
                    div()
                        .text_size(px(if compact { 29. } else { 34. }))
                        .line_height(px(if compact { 38. } else { 43. }))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(Label::new(post.title.clone())),
                );
            if let Some(note) = revisions
                .iter()
                .find(|group| group.head.thread_id() == post.hint().thread_id())
                .and_then(|group| revision_note(group.status))
            {
                article = article.child(
                    div()
                        .mt_2()
                        .text_sm()
                        .text_color(secondary(cx))
                        .child(Label::new(note)),
                );
            }
            if let Some(group) = revisions.iter().find(|group| {
                group.head.thread_id() == post.hint().thread_id()
                    && group.head.id != post.id
                    && group.status == RevisionStatus::Current
            }) {
                let latest = group.head.clone();
                article = article.child(
                    Button::new("new-revision")
                        .link()
                        .icon(mesh_icon("refresh-cw"))
                        .label(tr("查看新版本"))
                        .disabled(self.busy)
                        .on_click(cx.listener(move |this, _, _, cx| this.open(latest.clone(), cx))),
                );
            }
            article = article
                .child(
                    div()
                        .mt_2()
                        .text_xs()
                        .text_color(secondary(cx))
                        .child(Label::new(availability)),
                )
                .child(div().mt_5().child(post_markdown("body", post.body.clone())));
            if let Some(file) = post.hint().attachment() {
                let export = post.hint().clone();
                let keep = export.clone();
                let file_info = div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_shrink_0()
                            .size(px(36.))
                            .rounded_lg()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(surface(cx, 0xe1eee9, 0x263c37))
                            .child(mesh_icon("file").text_color(surface(cx, 0x2b6f5e, 0x9ad8c1))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .child(Label::new(file.name().to_owned())),
                    );
                let file_actions = div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        Button::new("export")
                            .ghost()
                            .icon(mesh_icon("arrow-down"))
                            .label(tr("下载"))
                            .accessibility_label(tr("下载附件"))
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.export_attachment(export.clone(), cx)
                            })),
                    )
                    .child(
                        Button::new("keep")
                            .ghost()
                            .icon(mesh_icon("hard-drive"))
                            .label(tr("离线保存"))
                            .accessibility_label(tr("离线保存附件"))
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.run_reader_transfer(
                                    ReaderTransfer::Attachment {
                                        hint: keep.clone(),
                                        destination: None,
                                    },
                                    cx,
                                );
                            })),
                    );
                article = article.child(
                    div()
                        .mt_6()
                        .p_3()
                        .rounded_lg()
                        .bg(surface(cx, 0xf1f5f1, 0x25302d))
                        .flex()
                        .gap_2()
                        .when(compact, |card| card.flex_col())
                        .when(!compact, |card| card.items_center())
                        .child(file_info)
                        .child(file_actions),
                );
                if let Some(feedback) = self.transfer.as_ref().filter(|state| {
                    matches!(&state.request, ReaderTransfer::Attachment { hint, .. } if hint.id == post.id)
                }) {
                    article = article.child(div().mt_2().child(self.transfer_feedback(feedback, cx)));
                }
            }
        } else {
            article = article
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .child(
                    div()
                        .size(px(50.))
                        .rounded_xl()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(surface(cx, 0xebf0fb, 0x262f43))
                        .child(mesh_icon("book-open").size(px(24.)).text_color(accent(cx))),
                )
                .when(!revisions.is_empty() || self.opening.is_some(), |pane| {
                    pane.child(div().text_sm().text_color(secondary(cx)).child(Label::new(
                        if self.opening.is_some() {
                            tr("正在打开…")
                        } else {
                            tr("选择一篇帖子阅读")
                        },
                    )))
                });
        }
        let detail = if self.selected.is_some() {
            detail.child(article.with_animation(
                "article-arrival",
                Animation::new(std::time::Duration::from_millis(145)).with_easing(|t| {
                    let decay = |t: f32| 1. - (1. + 6. * t) * (-6. * t).exp();
                    decay(t) / decay(1.)
                }),
                |article, t| article.opacity(0.90 + 0.10 * t),
            ))
        } else {
            detail.child(article)
        };
        if compact {
            div().size_full().flex().child(if self.selected.is_some() {
                detail.into_any_element()
            } else {
                list.into_any_element()
            })
        } else {
            div().size_full().flex().child(list).child(detail)
        }
    }
}

fn transfer_entry(id: &'static str, title: &'static str, detail: &'static str, cx: &App) -> Button {
    Button::new(id)
        .ghost()
        .h(px(60.))
        .w_full()
        .accessibility_label(title)
        .bg(surface(cx, 0xf1edf7, 0x272233))
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap_3()
                .child(mesh_icon("hard-drive").text_color(accent(cx)))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_start()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                        .child(div().text_xs().text_color(secondary(cx)).child(detail)),
                )
                .child(mesh_icon("chevron-right").text_color(secondary(cx))),
        )
}

fn feedback_message_requires_new_path(feedback: Option<&TransferFeedback>) -> bool {
    feedback.is_some_and(|feedback| feedback.message == "保存位置已有同名文件，请另选位置。")
}

fn apply_appearance(appearance: Appearance, window: Option<&mut Window>, cx: &mut App) {
    use gpui_kit::component::{Theme, ThemeMode};
    match appearance {
        Appearance::System => {
            #[cfg(feature = "desktop-visual")]
            if let Some(mode) = if std::env::var_os("MESH_FORUM_TEST_DARK").is_some() {
                Some(ThemeMode::Dark)
            } else if std::env::var_os("MESH_FORUM_TEST_LIGHT").is_some() {
                Some(ThemeMode::Light)
            } else {
                None
            } {
                Theme::change(mode, window, cx);
                return;
            }
            Theme::sync_system_appearance(window, cx);
        }
        Appearance::Light => Theme::change(ThemeMode::Light, window, cx),
        Appearance::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
}

fn article_menu_row(
    id: &'static str,
    label: &'static str,
    icon: &'static str,
    selected: bool,
) -> Button {
    Button::new(id)
        .ghost()
        .w_full()
        .accessibility_label(label)
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .child(mesh_icon(icon))
                .child(div().flex_1().child(label))
                .when(selected, |row| row.child(mesh_icon("check"))),
        )
}

fn article_button(
    id: &'static str,
    label: &'static str,
    icon: &'static str,
    busy: bool,
    cx: &App,
) -> Button {
    Button::new(id)
        .ghost()
        .size_8()
        .rounded_lg()
        .disabled(busy)
        .accessibility_label(label)
        .tooltip(label)
        .icon(mesh_icon(icon).size(px(17.)).text_color(accent(cx)))
}

fn mesh_icon(name: &'static str) -> Icon {
    Icon::default()
        .path(format!("icons/{name}.svg"))
        .size(px(15.))
}

fn accent(cx: &App) -> Hsla {
    surface(cx, 0x57469d, 0xc6b5fb).into()
}

fn secondary(cx: &App) -> Hsla {
    surface(cx, 0x535e72, 0xb3bed3).into()
}

const CANVAS: (u32, u32) = (0xfffdfa, 0x191820);
const BODY_TEXT: (u32, u32) = (0x232329, 0xe8e9ed);
const ERROR_TEXT: (u32, u32) = (0x9b292d, 0xffb4b7);
const TRANSFER_ERROR_SURFACE: (u32, u32) = (0xfff1f1, 0x38252d);
const ACTION_FILL: (u32, u32) = (0x5548b4, 0x705ecc);
const ACTION_HOVER: (u32, u32) = (0x493d9f, 0x7562cf);

fn field_error(message: &'static str, cx: &App) -> Div {
    let color = surface(cx, ERROR_TEXT.0, ERROR_TEXT.1);
    div()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        .child(mesh_icon("circle-alert").text_color(color))
        .child(Label::new(message).id(message).text_color(color))
}

fn page_title(title: &'static str) -> Div {
    div()
        .text_size(px(28.))
        .line_height(px(36.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(Label::new(title))
}

fn author_glyph(name: &str, cx: &App) -> Div {
    div()
        .size(px(30.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(surface(cx, 0xeee8f7, 0x382d4b))
        .text_color(accent(cx))
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .child(
            name.chars()
                .next()
                .unwrap_or('?')
                .to_uppercase()
                .to_string(),
        )
}

fn selection_button(cx: &App) -> ButtonCustomVariant {
    ButtonCustomVariant::new(cx)
        .color(surface(cx, 0xe8e0f7, 0x3a3053).into())
        .foreground(cx.theme().foreground)
        .hover(surface(cx, 0xdfd3f4, 0x473a61).into())
        .active(surface(cx, 0xd6c7eb, 0x534575).into())
}

fn brand_button(cx: &App) -> ButtonCustomVariant {
    ButtonCustomVariant::new(cx)
        .color(surface(cx, ACTION_FILL.0, ACTION_FILL.1).into())
        .foreground(rgb(0xffffff).into())
        .hover(surface(cx, ACTION_HOVER.0, ACTION_HOVER.1).into())
        .active(surface(cx, 0x3d3186, 0x5b49ad).into())
}

fn surface(cx: &App, light: u32, dark: u32) -> Rgba {
    rgb(if cx.theme().is_dark() { dark } else { light })
}

fn post_markdown(id: &'static str, body: String) -> TextView {
    TextView::markdown(id, body)
        .selectable(true)
        .image_source(|_| ImageSource::Custom(Arc::new(|_, _| None)))
        .link(|_, _| None)
}

async fn load_board(node: &Node, offset: u16) -> io::Result<Outcome> {
    let result = node.discover(offset).await;
    let history = node.cached_index().await;
    let (page, offline) = match result {
        Ok(page) if !page.is_empty() || history.is_empty() || offset != 0 => (page, false),
        Ok(_) | Err(_) if !history.is_empty() => (
            history
                .iter()
                .skip(usize::from(offset))
                .take(30)
                .cloned()
                .collect(),
            true,
        ),
        Ok(page) => (page, false),
        Err(error) => return Err(error),
    };
    Ok(Outcome::Board {
        page,
        history,
        offline,
    })
}

fn bind_search_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-f", FindPosts, Some("MeshForum")),
        KeyBinding::new("escape", DismissSearch, Some("MeshForumSearch")),
        KeyBinding::new("up", PreviousSearchHit, Some("MeshForumSearch")),
        KeyBinding::new("down", NextSearchHit, Some("MeshForumSearch")),
        KeyBinding::new("enter", OpenSearchHit, Some("MeshForumSearch")),
    ]);
}

pub fn run() -> io::Result<()> {
    let background_test = std::env::var_os("MESH_FORUM_TEST_BACKGROUND").is_some();
    if background_test && !cfg!(feature = "desktop-visual") {
        return Err(io::Error::other(
            "Background testing requires the desktop-visual feature",
        ));
    }
    let root = std::env::var_os("MESH_FORUM_DATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join("Library/Application Support/Mesh Forum"))
        })
        .ok_or_else(|| io::Error::other("missing application data directory"))?;
    let test_locale = background_test
        || (cfg!(feature = "desktop-visual")
            && std::env::var_os("MESH_FORUM_TEST_CHINESE").is_some());
    let preferences_path = root.join("desktop-preferences");
    let preferences = Preferences::load(&preferences_path);
    let listen: std::net::SocketAddr = std::env::var("MESH_FORUM_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:47624".into())
        .parse()
        .map_err(io::Error::other)?;
    let runtime = Runtime::new()?;
    let node = runtime.block_on(async {
        let mut node = Node::start_guest(&root, listen, [1; 32]).await?;
        if listen.is_ipv4() && listen.ip().is_unspecified() {
            let port = match std::env::var("MESH_FORUM_DISCOVERY_PORT") {
                Ok(port) => port.parse().map_err(io::Error::other)?,
                Err(_) => mesh_forum::forum::LAN_DISCOVERY_PORT,
            };
            node.enable_lan_discovery(port)?;
        }
        Ok::<_, io::Error>(node)
    })?;
    let node = Arc::new(node);
    let handle = runtime.handle().clone();
    let app = application().with_assets(ForumAssets);
    let app = if background_test {
        app.with_windowing(WindowingRequest::Headless)
    } else {
        app
    };
    app.run(move |cx| {
        init(cx);
        bind_search_keys(cx);
        localization::activate(preferences.language, test_locale);
        apply_appearance(preferences.appearance, None, cx);
        cx.set_reduce_motion(
            NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
                || (background_test && std::env::var_os("MESH_FORUM_TEST_REDUCE_MOTION").is_some()),
        );
        if let Some(main_thread) = MainThreadMarker::new() {
            let icon = NSData::with_bytes(APP_ICON);
            if let Some(image) = NSImage::initWithData(NSImage::alloc(), &icon) {
                // AppKit retains the image and both calls run on the main thread.
                unsafe {
                    NSApplication::sharedApplication(main_thread)
                        .setApplicationIconImage(Some(&image))
                };
            }
        }
        #[cfg(feature = "desktop-visual")]
        let width = if std::env::var_os("MESH_FORUM_TEST_NARROW").is_some() {
            680.
        } else {
            1080.
        };
        #[cfg(not(feature = "desktop-visual"))]
        let width = 1080.;
        let options = WindowOptions {
            focus: !background_test,
            show: !background_test,
            titlebar: Some(TitlebarOptions {
                title: Some("Mesh Forum".into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(width), px(720.)),
                cx,
            ))),
            window_min_size: Some(size(px(600.), px(500.))),
            ..Default::default()
        };
        match open_window(options, cx, move |window, cx| {
            cx.new(|cx| {
                Desktop::new(
                    node,
                    handle,
                    preferences,
                    preferences_path,
                    test_locale,
                    window,
                    cx,
                )
            })
        }) {
            Err(error) => {
                eprintln!("Could not open Mesh Forum: {error}");
                cx.quit();
            }
            Ok(_) if !background_test => cx.activate(true),
            Ok((handle, _)) => {
                if let Some(main_thread) = MainThreadMarker::new()
                    && let Some(window) = NSApplication::sharedApplication(main_thread)
                        .windows()
                        .firstObject()
                {
                    window.orderBack(None);
                }
                // A fully covered macOS window does not receive display-link frames.
                // Keep its accessibility tree current for semantic test input.
                cx.spawn(async move |cx| {
                    loop {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(250))
                            .await;
                        if handle
                            .update(cx, |_, window, cx| window.draw(cx).clear(cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod contrast_tests {
    use super::{ACTION_FILL, ACTION_HOVER, BODY_TEXT, CANVAS, ERROR_TEXT, TRANSFER_ERROR_SURFACE};

    fn luminance(rgb: u32) -> f64 {
        let channel = |shift: u32| {
            let encoded = f64::from((rgb >> shift) & 255) / 255.;
            if encoded <= 0.04045 {
                encoded / 12.92
            } else {
                ((encoded + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }

    #[test]
    fn foreground_colors_meet_normal_text_contrast_on_both_themes() {
        for (label, foreground, background) in [
            ("reader", BODY_TEXT, CANVAS),
            ("validation", ERROR_TEXT, CANVAS),
            ("transfer error", ERROR_TEXT, TRANSFER_ERROR_SURFACE),
            ("primary action", (0xffffff, 0xffffff), ACTION_FILL),
            ("hovered primary action", (0xffffff, 0xffffff), ACTION_HOVER),
        ] {
            for (ink, surface) in [(foreground.0, background.0), (foreground.1, background.1)] {
                let (ink, surface) = (luminance(ink), luminance(surface));
                let ratio = (ink.max(surface) + 0.05) / (ink.min(surface) + 0.05);
                assert!(ratio >= 4.5, "{label} contrast was {ratio:.2}:1");
            }
        }
    }
}

#[cfg(all(test, feature = "desktop-visual"))]
mod keyboard_tests {
    use super::{Desktop, Node, Preferences, Runtime, bind_search_keys, init};
    use gpui_kit::{self as gpui, TestAppContext};
    use std::sync::Arc;

    #[gpui::test]
    fn search_shortcuts_dispatch_with_the_search_input_focused(cx: &mut TestAppContext) {
        let profile = tempfile::tempdir().unwrap();
        let runtime = Runtime::new().unwrap();
        let (node, hits) = runtime.block_on(async {
            let node = Node::start(profile.path(), "127.0.0.1:0".parse().unwrap(), [1; 32])
                .await
                .unwrap();
            node.publish("One", "keyboard fixture").await.unwrap();
            node.publish("Two", "keyboard fixture").await.unwrap();
            let hits = node.search_local_posts("keyboard").await;
            (Arc::new(node), hits)
        });
        let expected = hits[1].hint.thread_id();
        cx.update(|cx| {
            init(cx);
            bind_search_keys(cx);
        });
        let (desktop, cx) = cx.add_window_view(|window, cx| {
            Desktop::new(
                node,
                runtime.handle().clone(),
                Preferences::default(),
                profile.path().join("desktop-preferences"),
                true,
                window,
                cx,
            )
        });
        cx.simulate_keystrokes("cmd-f");
        cx.update(|_, cx| {
            desktop.update(cx, |view, cx| {
                assert!(view.search_open);
                view.search_hits = hits;
                // The independent startup refresh is outside this input-dispatch test.
                view.operation = view.operation.wrapping_add(1);
                view.busy = false;
                cx.notify();
            })
        });
        cx.simulate_keystrokes("down");
        assert_eq!(cx.update(|_, cx| desktop.read(cx).search_selected), 1);
        cx.simulate_keystrokes("up");
        assert_eq!(cx.update(|_, cx| desktop.read(cx).search_selected), 0);
        cx.simulate_keystrokes("down enter");
        cx.update(|_, cx| {
            let view = desktop.read(cx);
            assert!(!view.search_open);
            assert!(
                view.opening == Some(expected)
                    || view
                        .selected
                        .as_ref()
                        .is_some_and(|post| post.hint().thread_id() == expected)
            );
        });
        cx.simulate_keystrokes("cmd-f escape");
        assert!(!cx.update(|_, cx| desktop.read(cx).search_open));
    }
}
