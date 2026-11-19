use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use codex_core::build_models_manager;
use codex_core::config::LoaderOverrides;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_login::AuthManager;
use codex_models_manager::bundled_models_response;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::openai_models::ModelPreset;
use codex_utils_cli::CliConfigOverrides;

/// Inspect and configure the default model.
#[derive(Debug, Parser)]
pub struct ModelCli {
    #[clap(flatten)]
    pub config_overrides: CliConfigOverrides,

    #[command(subcommand)]
    pub subcommand: ModelSubcommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum ModelSubcommand {
    /// List models available to the active provider and authentication mode.
    List(ListArgs),

    /// Set the default model in config.toml.
    Set(SetArgs),
}

#[derive(Debug, Parser)]
pub struct ListArgs {
    /// Include models hidden from the interactive picker.
    #[arg(long)]
    pub all: bool,

    /// Output model metadata as JSON.
    #[arg(long)]
    pub json: bool,

    /// Use only the model catalog bundled with this binary.
    #[arg(long)]
    pub bundled: bool,
}

#[derive(Debug, Parser)]
pub struct SetArgs {
    /// Model slug to persist as the default.
    #[arg(value_name = "MODEL")]
    pub model: String,
}

impl ModelCli {
    pub async fn run(self, loader_overrides: LoaderOverrides) -> Result<()> {
        let Self {
            config_overrides,
            subcommand,
        } = self;
        match subcommand {
            ModelSubcommand::List(args) => {
                run_list(args, &config_overrides, loader_overrides).await
            }
            ModelSubcommand::Set(args) => run_set(args, &config_overrides, loader_overrides).await,
        }
    }
}

async fn run_list(
    args: ListArgs,
    config_overrides: &CliConfigOverrides,
    loader_overrides: LoaderOverrides,
) -> Result<()> {
    let models = if args.bundled {
        // Validate overrides even though the bundled catalog does not use them.
        config_overrides
            .parse_overrides()
            .map_err(anyhow::Error::msg)?;
        bundled_model_presets()?
    } else {
        let config = crate::config_load::load_config(config_overrides, loader_overrides).await?;
        let auth_manager =
            AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ true).await?;
        let models_manager = build_models_manager(&config, auth_manager);
        models_manager
            .list_models(
                RefreshStrategy::OnlineIfUncached,
                config.http_client_factory(),
            )
            .await
    }
    .into_iter()
    .filter(|model| args.all || model.show_in_picker)
    .collect::<Vec<_>>();

    if args.json {
        serde_json::to_writer_pretty(std::io::stdout(), &models)?;
        println!();
    } else {
        for model in models {
            println!("{}", model.model);
        }
    }

    Ok(())
}

fn bundled_model_presets() -> Result<Vec<ModelPreset>> {
    let catalog = bundled_models_response()?;
    let mut models = catalog
        .models
        .into_iter()
        .map(ModelPreset::from)
        .collect::<Vec<_>>();
    ModelPreset::mark_default_by_picker_visibility(&mut models);
    Ok(models)
}

async fn run_set(
    args: SetArgs,
    config_overrides: &CliConfigOverrides,
    loader_overrides: LoaderOverrides,
) -> Result<()> {
    let model = args.model.trim();
    if model.is_empty() {
        bail!("model must not be empty");
    }

    let config = crate::config_load::load_config(config_overrides, loader_overrides).await?;
    let config_path = config
        .config_layer_stack
        .get_user_config_file()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| {
            config
                .codex_home
                .join(codex_config::CONFIG_TOML_FILE)
                .to_path_buf()
        });

    ConfigEditsBuilder::for_config(&config)
        .set_model_name(model)
        .apply()
        .await?;

    println!(
        "Set default model to `{model}` in {}.",
        config_path.display()
    );
    Ok(())
}
