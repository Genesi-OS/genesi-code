use settings::macros::define_settings_group;
use settings::{RespectUserSyncSetting, SupportedPlatforms, SyncToCloud};

define_settings_group!(CodeSettings, settings: [
    code_as_default_editor: CodeAsDefaultEditor {
        type: bool,
        default: false,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Never,
        private: false,
        toml_path: "code.editor.use_warp_as_default_editor",
        description: "Whether Genesi Code is used as the default code editor.",
    },
    autosave_enabled: AutosaveEnabled {
        type: bool,
        default: false,
        supported_platforms: SupportedPlatforms::DESKTOP,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        private: false,
        toml_path: "code.editor.autosave",
        description: "Whether IDE edits are automatically saved shortly after typing stops.",
    },
    codebase_context_enabled: CodebaseContextEnabled {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::DESKTOP,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        private: false,
        storage_key: "AgentModeCodebaseContext",
        toml_path: "code.indexing.agent_mode_codebase_context",
        description: "Whether codebase context is provided to the AI agent.",
    },
    auto_indexing_enabled: AutoIndexingEnabled {
        type: bool,
        default: false,
        supported_platforms: SupportedPlatforms::DESKTOP,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        private: false,
        storage_key: "AgentModeCodebaseContextAutoIndexing",
        toml_path: "code.indexing.agent_mode_codebase_context_auto_indexing",
        description: "Whether automatic codebase indexing is enabled.",
    },
    // Whether or not the user has manually dismissed the code toolbelt new feature popup.
    dismissed_code_toolbelt_new_feature_popup: DismissedCodeToolbeltNewFeaturePopup {
        type: bool,
        default: false,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        private: true,
    },
    // Controls whether the project explorer / file tree appears in the tools panel.
    show_project_explorer: ShowProjectExplorer {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        private: false,
        toml_path: "code.editor.show_project_explorer",
        description: "Whether the project explorer is shown in the tools panel.",
    },
    // Controls whether global file search appears in the tools panel.
    show_global_search: ShowGlobalSearch {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        private: false,
        toml_path: "code.editor.show_global_search",
        description: "Whether global file search is shown in the tools panel.",
    },
    // Controls whether hidden files (dotfiles) are shown in the project explorer.
    // On by default: in a code project the dotfiles are project files the user
    // edits -- .env, .gitignore, .github/, .eslintrc. Hiding them all made .env
    // look like it did not exist. Real noise is excluded elsewhere: .git is
    // marked ignored by is_git_internal_path, independently of this setting.
    show_hidden_files: ShowHiddenFiles {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        private: false,
        toml_path: "code.editor.show_hidden_files",
        description: "Whether hidden files (dotfiles) are shown in the project explorer.",
    },
    // AI inline completion: suggests the rest of the line as you type, from a
    // local model. Off by default -- it drives a model on every pause, and on a
    // machine already running Turbo that is a cost the user should opt into.
    ai_completion_enabled: AiCompletionEnabled {
        type: bool,
        default: false,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        private: false,
        toml_path: "code.ai_completion.enabled",
        description: "Whether the editor suggests code inline using a local AI model.",
    },
    // Empty means "whatever the AI panel is already using", which is the point:
    // one model stays loaded instead of a second one being pulled in behind it.
    ai_completion_model: AiCompletionModel {
        type: String,
        default: String::new(),
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        private: false,
        toml_path: "code.ai_completion.model",
        description: "Model used for inline completion. Empty follows the AI panel's model.",
    },
    // Turbo (llama-server) answers a short completion far faster than Ollama
    // does, which is the difference between a suggestion arriving before or
    // after the user has typed past it.
    ai_completion_use_turbo: AiCompletionUseTurbo {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        private: false,
        toml_path: "code.ai_completion.use_turbo",
        description: "Serve inline completions through Turbo rather than Ollama.",
    },
    // Speculative decoding is Turbo's own speed/accuracy trade. It is worth it
    // for completions, which are short and heavily constrained by the prefix.
    ai_completion_speculative_decoding: AiCompletionSpeculativeDecoding {
        type: bool,
        default: true,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        private: false,
        toml_path: "code.ai_completion.speculative_decoding",
        description: "Let Turbo use speculative decoding when generating inline completions.",
    },
    // Honours the AI Mode daemon: when it says the machine is not in an AI-ready
    // state, completions stay quiet rather than fighting it for the GPU.
    ai_completion_require_ai_mode: AiCompletionRequireAiMode {
        type: bool,
        default: false,
        supported_platforms: SupportedPlatforms::ALL,
        sync_to_cloud: SyncToCloud::Globally(RespectUserSyncSetting::Yes),
        private: false,
        toml_path: "code.ai_completion.require_ai_mode",
        description: "Only suggest completions while Genesi AI Mode is active.",
    },
]);
