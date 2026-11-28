use serde::{Deserialize, Serialize};
use std::{cell::Cell, io, path::Path};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    System,
    Chinese,
    English,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub language: Language,
    pub appearance: Appearance,
}

impl Preferences {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| bincode::deserialize(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(self, path: &Path) -> io::Result<()> {
        use io::Write as _;
        let folder = path
            .parent()
            .ok_or_else(|| io::Error::other("missing preferences folder"))?;
        let mut file = tempfile::NamedTempFile::new_in(folder)?;
        let bytes = bincode::serialize(&self).map_err(io::Error::other)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        Ok(())
    }
}

thread_local! {
    static ENGLISH: Cell<bool> = const { Cell::new(false) };
}

fn system_english(tags: impl IntoIterator<Item = impl AsRef<str>>) -> bool {
    for tag in tags {
        let tag = tag.as_ref();
        if let Some(base) = tag.split(['-', '_']).next() {
            if base.eq_ignore_ascii_case("zh") {
                return false;
            }
            if base.eq_ignore_ascii_case("en") {
                return true;
            }
        }
    }
    true
}

pub fn activate(language: Language, background_test: bool) {
    let english = match language {
        Language::English => true,
        Language::Chinese => false,
        Language::System if background_test => false,
        Language::System => {
            let languages = objc2_foundation::NSLocale::preferredLanguages();
            system_english(languages.iter().map(|language| language.to_string()))
        }
    };
    ENGLISH.set(english);
}

pub fn is_english() -> bool {
    ENGLISH.get()
}

pub fn tr(chinese: &str) -> &str {
    if is_english() {
        english(chinese)
    } else {
        chinese
    }
}

fn english(chinese: &str) -> &str {
    match chinese {
        "帖子标题" => "Post title",
        "写下你的帖子" => "Write your post",
        "显示名称" => "Display name",
        "密码" => "Password",
        "篇数" => "Post count",
        "容量（MiB）" => "Size (MiB)",
        "闲置天数" => "Idle days",
        "账号已恢复" => "Account restored",
        "已登录，可以发布草稿" => "Signed in. Your draft is ready.",
        "已保存并共享" => "Saved and shared",
        "已保存到本机" => "Saved on this device",
        "已取消书签" => "Bookmark removed",
        "已发布到本机" => "Published on this device",
        "已撤回" => "Post withdrawn",
        "正在加载帖子…" => "Loading posts…",
        "正在读取存储用量…" => "Reading storage usage…",
        "正在下载并校验正文…" => "Downloading and verifying post…",
        "正在保存帖子…" => "Publishing post…",
        "附件文件名无效" => "Invalid attachment filename",
        "正在更新书签…" => "Updating bookmark…",
        "备份账号" => "Back up account",
        "从备份恢复" => "Restore from backup",
        "加密身份文件" => "Encrypted identity file",
        "换设备后可用原账号继续发帖和编辑。恢复时需要备份生成时的账号密码。" => {
            "Keep publishing and editing as the same author on another device. You’ll need the password used when the backup was created."
        }
        "帖子、书签和附件留在原设备。" => {
            "Posts, bookmarks, and attachments stay on the original device."
        }
        "选择保存位置" => "Choose save location",
        "备份文件" => "Backup file",
        "尚未选择文件" => "No file selected",
        "更换文件" => "Change file",
        "选择备份文件" => "Choose backup file",
        "备份生成时的账号密码" => "Password used when the backup was created",
        "恢复账号" => "Restore account",
        "返回账号" => "Back to account",
        "正在恢复…" => "Restoring…",
        "正在解锁账号…" => "Unlocking account…",
        "选择账号备份" => "Choose account backup",
        "无法打开文件选择器，请重试。" => "Couldn’t open the file picker. Try again.",
        "正在保存账号备份…" => "Saving account backup…",
        "加密账号备份已保存" => "Encrypted account backup saved",
        "无法打开保存面板，请重试。" => "Couldn’t open the save dialog. Try again.",
        "正在退出账号…" => "Signing out…",
        "选择附件" => "Choose attachment",
        "正在下载附件…" => "Downloading attachment…",
        "附件已下载" => "Attachment downloaded",
        "正在检测节点…" => "Checking nearby devices…",
        "正在更新上传设置…" => "Updating sharing…",
        "上传已暂停" => "Sharing paused",
        "上传已恢复" => "Sharing resumed",
        "写帖子" => "New post",
        "版面" => "Board",
        "我的帖子" => "My posts",
        "书签" => "Bookmarks",
        "已暂停上传" => "Sharing paused",
        "节点" => "Devices",
        "存储" => "Storage",
        "账号" => "Account",
        "编辑帖子" => "Edit post",
        "关闭" => "Close",
        "发布时登录。" => "Sign in when you’re ready to publish.",
        "请填写帖子标题" => "Enter a post title",
        "撰写" => "Write",
        "预览" => "Preview",
        "请填写帖子正文" => "Enter the post body",
        "添加附件" => "Add attachment",
        "移除附件" => "Remove attachment",
        "发布" => "Publish",
        "附近节点" => "Nearby devices",
        "正在自动发现同一局域网内的设备" => {
            "Discovering nearby devices on your local network"
        }
        "恢复上传" => "Resume sharing",
        "暂停上传" => "Pause sharing",
        "检测连接" => "Check connections",
        "最近检测可达" => "Reachable",
        "可达 · 已暂停上传" => "Reachable · Sharing paused",
        "未连通" => "Unreachable",
        "本机发帖身份的加密副本" => "Encrypted copy of your author identity",
        "退出登录" => "Sign out",
        "登录" => "Sign in",
        "创建账号" => "Create account",
        "发布帖子需要登录。浏览和收藏无需账号。" => {
            "Read and bookmark as a guest. Sign in to publish."
        }
        "返回草稿" => "Back to draft",
        "在此设备继续使用原来的发帖身份" => {
            "Keep using your existing author identity"
        }
        "已校验下载进度" => "Verified download progress",
        "存储与缓存" => "Storage and cache",
        "离线书签" => "Offline bookmarks",
        "浏览缓存" => "Browsing cache",
        "下载暂存" => "Partial downloads",
        "下载中" => "Downloading",
        "自动清理" => "Automatic cleanup",
        "这里的设置只影响浏览缓存。" => "These settings apply to the browsing cache.",
        "保存设置" => "Save settings",
        "请输入有效的非负整数" => "Enter a valid non-negative whole number",
        "正在更新缓存…" => "Updating cache…",
        "更多操作" => "More actions",
        "帖子书签" => "Post bookmark",
        "保存到书签" => "Save bookmark",
        "撤回帖子" => "Withdraw post",
        "保存并共享" => "Save and share",
        "仅本地保存" => "Save locally",
        "取消书签" => "Remove bookmark",
        "编辑" => "Edit",
        "撤回这篇帖子？" => "Withdraw this post?",
        "收到撤回通知的设备会移除这篇帖子。" => {
            "Devices that receive the withdrawal will remove this post."
        }
        "取消" => "Cancel",
        "撤回" => "Withdraw",
        "正在撤回…" => "Withdrawing post…",
        "刷新" => "Refresh",
        "上一页" => "Previous page",
        "下一页" => "Next page",
        "缺少中间版本" => "Missing revision",
        "版本有冲突" => "Revision conflict",
        "附近帖子" => "Nearby posts",
        "离线索引 · 来源待确认" => "Offline index · Source unconfirmed",
        "旧版作者" => "Legacy author",
        "正在加载…" => "Loading…",
        "收藏的帖子会出现在这里。" => "Your bookmarks will appear here.",
        "你发布的帖子会出现在这里。" => "Posts you publish will appear here.",
        "暂无附近帖子。发现设备后会自动更新。" => {
            "No nearby posts yet. Updates appear as devices are discovered."
        }
        "返回列表" => "Back to list",
        "查看新版本" => "Open latest revision",
        "下载" => "Download",
        "下载附件" => "Download attachment",
        "离线保存" => "Save offline",
        "离线保存附件" => "Save attachment offline",
        "正在保存附件…" => "Saving attachment…",
        "附件已离线保存" => "Attachment saved offline",
        "正在打开…" => "Opening…",
        "选择一篇帖子阅读" => "Select a post to read",
        "设置" => "Settings",
        "外观与语言" => "Appearance and language",
        "界面语言" => "App language",
        "跟随系统" => "System",
        "浅色" => "Light",
        "深色" => "Dark",
        "主题" => "Theme",
        "查找帖子" => "Find posts",
        "搜索标题、作者或正文" => "Search title, author, or body",
        "只搜索本机的帖子与缓存。" => "Searches only posts and cache on this device.",
        "没有找到匹配的本机帖子。" => "No matching posts on this device.",
        "输入关键词开始搜索。" => "Enter a search term to get started.",
        "正在搜索…" => "Searching…",
        "未能读取帖子。请确认附近设备在线，然后重试。" => {
            "Couldn’t open the post. Make sure a nearby device is online and try again."
        }
        "未能下载附件。请确认附近设备在线，然后重试。" => {
            "Couldn’t download the attachment. Make sure a nearby device is online and try again."
        }
        "下载校验失败，请重试。" => "Verification failed. Try again.",
        "重试" => "Retry",
        "可离线阅读" => "Available offline",
        "已缓存在本机" => "Cached on this device",
        "本机副本已清理。将尝试从附近设备重新下载。" => {
            "The local copy was cleared. Trying to download it from a nearby device."
        }
        "密码错误或账号文件已损坏" => "Wrong password or damaged account file",
        "设备上已有账号" => "This device already has an account",
        "账号格式不受支持" => "Unsupported account format",
        "密码过长" => "Password is too long",
        "帖子缺少已签名的作者账号" => "The post has no signed author identity",
        "无法保存偏好设置。" => "Couldn’t save your preferences.",
        "设备" => "Device",
        "篇" => "posts",
        "最多缓存篇数" => "Maximum cached posts",
        "缓存容量" => "Cache size",
        "闲置回收天数" => "Days before idle cleanup",
        "清理缓存" => "Clear cache",
        "来源待确认" => "Source unconfirmed",
        "保存位置已有同名文件，请另选位置。" => {
            "A file with that name already exists. Choose a different location."
        }
        "操作未完成，请重试。" => "Couldn’t complete the operation. Try again.",
        "本机副本已清理。可从在线附近设备重新下载。" => {
            "The local copy was cleared. Try downloading it again from a nearby device."
        }
        "正在查看临时副本" => "Viewing a temporary copy",
        "请先登录再发布或修改帖子" => "Sign in before publishing or editing posts",
        "请先登录再备份账号" => "Sign in before backing up your account",
        "请输入有效的名字和至少 8 个字符的密码" => {
            "Enter a valid name and a password with at least 8 characters"
        }
        "账号数据无效" => "Invalid account data",
        "帖子标题或正文超出限制。" => "The post title or body exceeds its limit.",
        "应用没有权限读取或保存这个文件。" => {
            "Mesh Forum doesn’t have permission to read or save this file."
        }
        "数据与连接" => "Data and connections",
        "完成" => "Done",
        "关闭搜索" => "Close search",
        _ => chinese,
    }
}

pub fn user_error(error: &io::Error) -> String {
    let message = error.to_string();
    if english(&message) != message {
        return message;
    }
    match (error.kind(), message.as_str()) {
        (io::ErrorKind::InvalidInput, "invalid post size or title") => "帖子标题或正文超出限制。",
        (io::ErrorKind::PermissionDenied, _) => "应用没有权限读取或保存这个文件。",
        _ => "操作未完成，请重试。",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_uses_first_supported_language_and_falls_back_to_english() {
        assert!(!system_english(["fr-FR", "zh-Hans-CN", "en-US"]));
        assert!(system_english(["en-GB", "zh-CN"]));
        assert!(system_english(["de-DE"]));
    }

    #[test]
    fn preferences_round_trip_and_invalid_file_uses_system_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop-preferences");
        Preferences {
            language: Language::English,
            appearance: Appearance::Dark,
        }
        .save(&path)
        .unwrap();
        let settings = Preferences::load(&path);
        assert_eq!(settings.language, Language::English);
        assert_eq!(settings.appearance, Appearance::Dark);
        std::fs::write(&path, b"invalid").unwrap();
        assert_eq!(Preferences::load(&path).language, Language::System);
    }
}
