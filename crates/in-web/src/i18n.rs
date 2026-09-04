//! UI strings in two languages.
//!
//! A signed-in reader's language comes off their own user row, the way their
//! theme and interface do; everybody else reads `Accept-Language`. An
//! unrecognized tag or code is not a refusal, just the default.

use topcoat::context::Cx;
use topcoat::router::header;
use topcoat::router::request::headers;

/// English or Turkish: the reader's own language when signed in, the
/// request's `Accept-Language` otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Tr,
}

impl Lang {
    /// Anything not `tr` is English.
    pub fn from_code(code: &str) -> Lang {
        match code {
            "tr" => Lang::Tr,
            _ => Lang::En,
        }
    }

    /// The first language tag the browser asked for: `tr`, `tr-TR` and
    /// friends read Turkish, the rest — including a missing header — read
    /// English.
    pub fn from_header(value: &str) -> Lang {
        let tag = value.split(',').next().unwrap_or("").trim();
        let tag = tag.split(';').next().unwrap_or("").trim();
        let base = tag.split('-').next().unwrap_or("").trim().to_lowercase();
        Lang::from_code(&base)
    }

    /// The `<html lang>` value.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Tr => "tr",
        }
    }
}

/// The request's language: the signed-in reader's own row first,
/// `Accept-Language` when signed out or unreadable.
///
/// Cheap per call: [`crate::server::current_user`] is memoized per request,
/// and every signed-in page already resolved the user before asking.
pub async fn lang(cx: &Cx) -> Lang {
    let signed_in = match crate::server::current_user(cx).await {
        Ok(user) => user.as_ref().map(|user| Lang::from_code(&user.language)),
        Err(_) => None,
    };
    if let Some(language) = signed_in {
        return language;
    }
    headers(cx)
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok())
        .map(Lang::from_header)
        .unwrap_or(Lang::En)
}

/// One variant per UI phrase. A typo'd key fails to compile rather than
/// falling through to nothing at render time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    // Shared chrome.
    Cancel,
    Save,
    Close,
    Delete,
    Rename,
    Move,
    Create,
    Confirm,
    Back,
    Download,
    Retry,
    SomethingWentWrong,
    NothingAtThisAddress,
    BackToDrive,
    // Landing.
    WelcomeTitle,
    WelcomeBlurb,
    SignIn,
    // Topbar.
    NavDrive,
    NavShared,
    NavTrash,
    NavSearch,
    NavSettings,
    SignOut,
    AdminBadge,
    UserMenuLabel,
    // Drive browser.
    Drive,
    NewFolder,
    FolderName,
    CreateFolder,
    RenameFolder,
    MoveFolder,
    DeleteFolder,
    EmptyFolder,
    FoldersHeading,
    FilesHeading,
    NameColumn,
    SizeColumn,
    ModifiedColumn,
    // Upload.
    Upload,
    UploadFile,
    ChooseFiles,
    DropFilesHere,
    Uploading,
    UploadComplete,
    UploadFailed,
    CancelUpload,
    RetryUpload,
    // File view.
    FileDetails,
    PreviewUnavailable,
    RenameFile,
    MoveFile,
    DeleteFile,
    // Share.
    Share,
    ShareLink,
    CopyLink,
    RevokeLink,
    SharedWith,
    ShareWithUser,
    SharePlaceholder,
    CanDownload,
    RemoveAccess,
    SharedWithMe,
    NoSharedItems,
    // Trash.
    Trash,
    TrashEmpty,
    Restore,
    DeleteForever,
    EmptyTrash,
    // Search.
    SearchResults,
    SearchPlaceholder,
    NoResults,
    TypeToSearch,
    // Settings.
    Settings,
    Profile,
    DisplayName,
    EmailAddress,
    QuotaUsage,
    QuotaOf,
    AdminPanel,
    AllUsers,
    SetQuota,
    QuotaBytes,
    DisableUser,
    EnableUser,
    DisabledBadge,
    // Wave-2 share/trash/search/settings additions.
    LinkCreated,
    CopyLinkOnce,
    ExpiresLabel,
    NeverExpires,
    ViewOnly,
    NoLinks,
    ManageLinks,
    UiLabel,
    ThemeLabel,
    LightOption,
    DarkOption,
    LanguageLabel,
    Saved,
}

pub fn t(lang: Lang, key: Key) -> &'static str {
    use Key::*;
    match lang {
        Lang::En => match key {
            Cancel => "Cancel",
            Save => "Save",
            Close => "Close",
            Delete => "Delete",
            Rename => "Rename",
            Move => "Move",
            Create => "Create",
            Confirm => "Confirm",
            Back => "Back",
            Download => "Download",
            Retry => "Retry",
            SomethingWentWrong => "Something went wrong.",
            NothingAtThisAddress => "There is nothing at this address.",
            BackToDrive => "Back to the drive",
            WelcomeTitle => "In — your files",
            WelcomeBlurb => "Your folders and files, shared on your terms.",
            SignIn => "Sign in",
            NavDrive => "Drive",
            NavShared => "Shared",
            NavTrash => "Trash",
            NavSearch => "Search",
            NavSettings => "Settings",
            SignOut => "Sign out",
            AdminBadge => "admin",
            UserMenuLabel => "Account",
            Drive => "Drive",
            NewFolder => "New folder",
            FolderName => "Folder name",
            CreateFolder => "Create folder",
            RenameFolder => "Rename folder",
            MoveFolder => "Move folder",
            DeleteFolder => "Delete folder",
            EmptyFolder => "This folder is empty.",
            FoldersHeading => "Folders",
            FilesHeading => "Files",
            NameColumn => "Name",
            SizeColumn => "Size",
            ModifiedColumn => "Changed",
            Upload => "Upload",
            UploadFile => "Upload files",
            ChooseFiles => "Choose files",
            DropFilesHere => "Drop files here to upload them.",
            Uploading => "Uploading…",
            UploadComplete => "Upload complete.",
            UploadFailed => "The upload failed.",
            CancelUpload => "Cancel the upload",
            RetryUpload => "Try again",
            FileDetails => "File",
            PreviewUnavailable => "No preview for this kind of file.",
            RenameFile => "Rename file",
            MoveFile => "Move file",
            DeleteFile => "Delete file",
            Share => "Share",
            ShareLink => "Share link",
            CopyLink => "Copy the link",
            RevokeLink => "Revoke the link",
            SharedWith => "Shared with",
            ShareWithUser => "Share with someone",
            SharePlaceholder => "Name or address",
            CanDownload => "May download",
            RemoveAccess => "Remove access",
            SharedWithMe => "Shared with me",
            NoSharedItems => "Nothing is shared with you yet.",
            Trash => "Trash",
            TrashEmpty => "The trash is empty.",
            Restore => "Restore",
            DeleteForever => "Delete forever",
            EmptyTrash => "Empty the trash",
            SearchResults => "Search results",
            SearchPlaceholder => "Search files and folders…",
            NoResults => "Nothing found.",
            TypeToSearch => "Type above to search your files.",
            Settings => "Settings",
            Profile => "Profile",
            DisplayName => "Name",
            EmailAddress => "Address",
            QuotaUsage => "Storage used",
            QuotaOf => "of",
            AdminPanel => "Everyone",
            AllUsers => "Everyone",
            SetQuota => "Set the quota",
            QuotaBytes => "Quota in bytes",
            DisableUser => "Disable",
            EnableUser => "Enable",
            DisabledBadge => "disabled",
            LinkCreated => "Link created.",
            CopyLinkOnce => "Copy it now — it is shown once and never again.",
            ExpiresLabel => "Expires",
            NeverExpires => "Never",
            ViewOnly => "View only — preview, no downloads",
            NoLinks => "No share links yet.",
            ManageLinks => "Share links",
            UiLabel => "Interface",
            ThemeLabel => "THEME",
            LightOption => "Light",
            DarkOption => "Dark",
            LanguageLabel => "LANGUAGE",
            Saved => "Saved.",
        },
        Lang::Tr => match key {
            Cancel => "Vazgeç",
            Save => "Kaydet",
            Close => "Kapat",
            Delete => "Sil",
            Rename => "Yeniden adlandır",
            Move => "Taşı",
            Create => "Oluştur",
            Confirm => "Onayla",
            Back => "Geri",
            Download => "İndir",
            Retry => "Tekrar dene",
            SomethingWentWrong => "Bir şeyler ters gitti.",
            NothingAtThisAddress => "Bu adreste bir şey yok.",
            BackToDrive => "Sürücüye dön",
            WelcomeTitle => "In — dosyaların",
            WelcomeBlurb => "Klasörlerin ve dosyaların, senin kurallarınla.",
            SignIn => "Oturum aç",
            NavDrive => "Sürücü",
            NavShared => "Paylaşılan",
            NavTrash => "Çöp",
            NavSearch => "Ara",
            NavSettings => "Ayarlar",
            SignOut => "Oturumu kapat",
            AdminBadge => "yönetici",
            UserMenuLabel => "Hesap",
            Drive => "Sürücü",
            NewFolder => "Yeni klasör",
            FolderName => "Klasör adı",
            CreateFolder => "Klasörü oluştur",
            RenameFolder => "Klasörü yeniden adlandır",
            MoveFolder => "Klasörü taşı",
            DeleteFolder => "Klasörü sil",
            EmptyFolder => "Bu klasör boş.",
            FoldersHeading => "Klasörler",
            FilesHeading => "Dosyalar",
            NameColumn => "Ad",
            SizeColumn => "Boyut",
            ModifiedColumn => "Değişiklik",
            Upload => "Yükle",
            UploadFile => "Dosya yükle",
            ChooseFiles => "Dosya seç",
            DropFilesHere => "Yüklemek için dosyaları buraya bırak.",
            Uploading => "Yükleniyor…",
            UploadComplete => "Yükleme tamamlandı.",
            UploadFailed => "Yükleme başarısız oldu.",
            CancelUpload => "Yüklemeyi iptal et",
            RetryUpload => "Yeniden dene",
            FileDetails => "Dosya",
            PreviewUnavailable => "Bu tür dosyanın önizlemesi yok.",
            RenameFile => "Dosyayı yeniden adlandır",
            MoveFile => "Dosyayı taşı",
            DeleteFile => "Dosyayı sil",
            Share => "Paylaş",
            ShareLink => "Paylaşım bağlantısı",
            CopyLink => "Bağlantıyı kopyala",
            RevokeLink => "Bağlantıyı iptal et",
            SharedWith => "Paylaşılanlar",
            ShareWithUser => "Biriyle paylaş",
            SharePlaceholder => "Ad ya da adres",
            CanDownload => "İndirebilir",
            RemoveAccess => "Erişimi kaldır",
            SharedWithMe => "Benimle paylaşılanlar",
            NoSharedItems => "Seninle henüz bir şey paylaşılmadı.",
            Trash => "Çöp",
            TrashEmpty => "Çöp boş.",
            Restore => "Geri al",
            DeleteForever => "Kalıcı olarak sil",
            EmptyTrash => "Çöpü boşalt",
            SearchResults => "Arama sonuçları",
            SearchPlaceholder => "Dosya ve klasör ara…",
            NoResults => "Bir şey bulunamadı.",
            TypeToSearch => "Dosyalarında aramak için yukarı yaz.",
            Settings => "Ayarlar",
            Profile => "Profil",
            DisplayName => "Ad",
            EmailAddress => "Adres",
            QuotaUsage => "Kullanılan alan",
            QuotaOf => "/",
            AdminPanel => "Herkes",
            AllUsers => "Herkes",
            SetQuota => "Kotayı ayarla",
            QuotaBytes => "Bayt olarak kota",
            DisableUser => "Devre dışı bırak",
            EnableUser => "Etkinleştir",
            DisabledBadge => "devre dışı",
            LinkCreated => "Bağlantı oluşturuldu.",
            CopyLinkOnce => "Şimdi kopyala — bir kez gösterilir, bir daha gösterilmez.",
            ExpiresLabel => "Bitiş",
            NeverExpires => "Süresiz",
            ViewOnly => "Yalnızca görüntüleme — önizleme, indirme yok",
            NoLinks => "Henüz paylaşım bağlantısı yok.",
            ManageLinks => "Paylaşım bağlantıları",
            UiLabel => "Arayüz",
            ThemeLabel => "TEMA",
            LightOption => "Açık",
            DarkOption => "Koyu",
            LanguageLabel => "DİL",
            Saved => "Kaydedildi.",
        },
    }
}
