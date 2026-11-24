//! Server settings an admin changes in the app. Each follows its
//! environment variable until it is changed, and again once it is set back
//! to the same value; the mode and the score kind are stored once and then
//! only changed in the app.

use sqlx::PgExecutor;
use starbooru_core::{
    AiGeneralUpdate, AiMode, AiPreset, AiTool, AiToolUpdate, BackupSchedule, Mode, ScoreKind,
    ServerSettings, UpdateSettings,
};

use crate::ai::{AiSetup, Provider, ToolSetup};
use crate::config::Config;
use crate::error::AppResult;
use crate::state::AppState;

/// name, allow_registration, trash_days, auto_meta_tags, mode, score_kind;
/// NULL follows the environment.
type SettingsRow = (
    Option<String>,
    Option<bool>,
    Option<i32>,
    Option<bool>,
    Option<String>,
    Option<String>,
);

/// The settings in force.
pub async fn current(state: &AppState) -> AppResult<ServerSettings> {
    let row: Option<SettingsRow> = sqlx::query_as(
        "SELECT name, allow_registration, trash_days, auto_meta_tags, mode, score_kind \
         FROM server_settings",
    )
    .fetch_optional(&state.db)
    .await?;
    let (name, allow_registration, trash_days, auto_meta_tags, mode, kind) =
        row.unwrap_or_default();
    let config = &state.config;
    let (mode, score_kind) = in_force(mode, kind);
    Ok(ServerSettings {
        name: name.unwrap_or_else(|| config.instance_name.clone()),
        allow_registration: allow_registration.unwrap_or(config.allow_registration),
        trash_days: trash_days
            .and_then(|d| u32::try_from(d).ok())
            .unwrap_or(config.trash_days),
        auto_meta_tags: auto_meta_tags.unwrap_or(config.auto_meta_tags),
        mode,
        score_kind,
        changes: Vec::new(),
    })
}

/// The mode and the score kind in force, from what is stored. Unlike the
/// other settings they don't follow the environment: `STARBOORU_MODE` and
/// `STARBOORU_SCORES` only settle them on the first start
/// (`modes::settle`), and NULL before that is what the data was until
/// then, private with likes. A public instance counts likes, whatever is
/// stored.
fn in_force(mode: Option<String>, kind: Option<String>) -> (Mode, ScoreKind) {
    let mode = mode.as_deref().and_then(Mode::parse).unwrap_or_default();
    let kind = kind
        .as_deref()
        .and_then(ScoreKind::parse)
        .unwrap_or_default()
        .in_mode(mode);
    (mode, kind)
}

/// The mode and the score kind as stored, `None` where not settled yet.
pub async fn stored_scoring(
    db: impl PgExecutor<'_>,
) -> AppResult<(Option<Mode>, Option<ScoreKind>)> {
    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT mode, score_kind FROM server_settings")
            .fetch_optional(db)
            .await?;
    let (mode, kind) = row.unwrap_or_default();
    Ok((
        mode.as_deref().and_then(Mode::parse),
        kind.as_deref().and_then(ScoreKind::parse),
    ))
}

/// The mode and the score kind in force, read through `db`, so a change can
/// read them inside its transaction.
pub async fn scoring(db: impl PgExecutor<'_>) -> AppResult<(Mode, ScoreKind)> {
    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT mode, score_kind FROM server_settings")
            .fetch_optional(db)
            .await?;
    let (mode, kind) = row.unwrap_or_default();
    Ok(in_force(mode, kind))
}

/// The score kind a private instance would count: what is stored, whatever
/// the mode.
pub async fn chosen_score_kind(state: &AppState) -> AppResult<ScoreKind> {
    Ok(stored_scoring(&state.db).await?.1.unwrap_or_default())
}

/// The mode in force.
pub async fn mode(state: &AppState) -> AppResult<Mode> {
    Ok(scoring(&state.db).await?.0)
}

/// The score kind in force.
pub async fn score_kind(state: &AppState) -> AppResult<ScoreKind> {
    Ok(scoring(&state.db).await?.1)
}

/// Saves the mode, always written out: the variable mustn't move it again.
pub async fn save_mode(db: impl PgExecutor<'_>, mode: Mode) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO server_settings (id, mode) VALUES (true, $1) \
         ON CONFLICT (id) DO UPDATE SET mode = $1, updated_at = now()",
    )
    .bind(mode.as_str())
    .execute(db)
    .await?;
    Ok(())
}

/// Saves the score kind, always written out, like the mode.
pub async fn save_score_kind(db: impl PgExecutor<'_>, kind: ScoreKind) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO server_settings (id, score_kind) VALUES (true, $1) \
         ON CONFLICT (id) DO UPDATE SET score_kind = $1, updated_at = now()",
    )
    .bind(kind.as_str())
    .execute(db)
    .await?;
    Ok(())
}

/// Saves `settings` (already cleaned), but not the mode or the score kind.
/// Values equal to the environment's are stored as unset, so a later change
/// to the variable still applies.
pub async fn save(
    db: impl PgExecutor<'_>,
    config: &Config,
    settings: &ServerSettings,
) -> AppResult<()> {
    let trash_days = i32::try_from(settings.trash_days).unwrap_or(i32::MAX);
    sqlx::query(
        "INSERT INTO server_settings (id, name, allow_registration, trash_days, auto_meta_tags) \
         VALUES (true, $1, $2, $3, $4) \
         ON CONFLICT (id) DO UPDATE SET name = $1, allow_registration = $2, \
         trash_days = $3, auto_meta_tags = $4, updated_at = now()",
    )
    .bind((settings.name != config.instance_name).then_some(&settings.name))
    .bind(
        (settings.allow_registration != config.allow_registration)
            .then_some(settings.allow_registration),
    )
    .bind((settings.trash_days != config.trash_days).then_some(trash_days))
    .bind((settings.auto_meta_tags != config.auto_meta_tags).then_some(settings.auto_meta_tags))
    .execute(db)
    .await?;
    Ok(())
}

/// The automatic backup schedule in force.
pub async fn backup_schedule(state: &AppState) -> AppResult<BackupSchedule> {
    let row: Option<(Option<i32>, Option<i32>)> =
        sqlx::query_as("SELECT backup_days, backup_keep FROM server_settings")
            .fetch_optional(&state.db)
            .await?;
    let (days, keep) = row.unwrap_or_default();
    let config = &state.config;
    Ok(BackupSchedule {
        every_days: days
            .and_then(|d| u32::try_from(d).ok())
            .unwrap_or(config.backup_days),
        keep: keep
            .and_then(|k| u32::try_from(k).ok())
            .unwrap_or(config.backup_keep),
    })
}

/// Saves `schedule` (already checked), like [`save`].
pub async fn save_backup_schedule(state: &AppState, schedule: BackupSchedule) -> AppResult<()> {
    let config = &state.config;
    let days = i32::try_from(schedule.every_days).unwrap_or(i32::MAX);
    let keep = i32::try_from(schedule.keep).unwrap_or(i32::MAX);
    sqlx::query(
        "INSERT INTO server_settings (id, backup_days, backup_keep) VALUES (true, $1, $2) \
         ON CONFLICT (id) DO UPDATE SET backup_days = $1, backup_keep = $2, updated_at = now()",
    )
    .bind((schedule.every_days != config.backup_days).then_some(days))
    .bind((schedule.keep != config.backup_keep).then_some(keep))
    .execute(&state.db)
    .await?;
    Ok(())
}

/// The update checks and automatic updates in force.
pub async fn updates(state: &AppState) -> AppResult<UpdateSettings> {
    let row: Option<(Option<bool>, Option<bool>, Option<i16>)> =
        sqlx::query_as("SELECT update_check, auto_update, auto_update_hour FROM server_settings")
            .fetch_optional(&state.db)
            .await?;
    let (check, auto, hour) = row.unwrap_or_default();
    let config = &state.config.updates;
    Ok(UpdateSettings {
        check: check.unwrap_or(config.check),
        auto: auto.unwrap_or(config.auto),
        auto_hour: hour
            .and_then(|h| u8::try_from(h).ok())
            .filter(|h| *h <= 23)
            .unwrap_or(config.auto_hour),
    })
}

/// Saves `settings` (already checked), like [`save`].
pub async fn save_updates(state: &AppState, settings: UpdateSettings) -> AppResult<()> {
    let config = &state.config.updates;
    sqlx::query(
        "INSERT INTO server_settings (id, update_check, auto_update, auto_update_hour) \
         VALUES (true, $1, $2, $3) \
         ON CONFLICT (id) DO UPDATE SET update_check = $1, auto_update = $2, \
         auto_update_hour = $3, updated_at = now()",
    )
    .bind((settings.check != config.check).then_some(settings.check))
    .bind((settings.auto != config.auto).then_some(settings.auto))
    .bind((settings.auto_hour != config.auto_hour).then_some(i16::from(settings.auto_hour)))
    .execute(&state.db)
    .await?;
    Ok(())
}

/// The AI setup in force: what the admin saved over `STARBOORU_AI_*`.
pub async fn ai(state: &AppState) -> AppResult<AiSetup> {
    #[derive(sqlx::FromRow)]
    struct Row {
        tool: String,
        mode: String,
        preset: String,
        model: String,
        api_url: String,
        api_key: String,
        api_model: String,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT tool, mode, preset, model, api_url, api_key, api_model FROM ai_tools",
    )
    .fetch_all(&state.db)
    .await?;
    let general: Option<(Option<bool>, Option<String>)> =
        sqlx::query_as("SELECT ai_autotag, ai_auto_translate FROM server_settings")
            .fetch_optional(&state.db)
            .await?;
    let (autotag, auto_translate) = general.unwrap_or_default();
    let env = &state.config.ai;
    let tools = AiTool::ALL
        .into_iter()
        .map(|tool| {
            let env_mode = match tool {
                AiTool::Tagging => env.tagging,
                AiTool::Translation => env.translation,
                AiTool::Comics => env.comics,
            };
            let default_mode = env_mode.unwrap_or(if !env.url.is_empty() {
                AiMode::Api
            } else if env.local_default {
                AiMode::Local
            } else {
                AiMode::Off
            });
            let Some(row) = rows.iter().find(|r| r.tool == tool.as_str()) else {
                return ToolSetup {
                    tool,
                    mode: default_mode,
                    preset: AiPreset::default(),
                    model: String::new(),
                    api: Provider::new(env.url.clone(), env.key.clone(), env.model.clone()),
                };
            };
            let follows_env = row.api_url.is_empty() || row.api_url == env.url;
            ToolSetup {
                tool,
                mode: AiMode::parse(&row.mode).unwrap_or(default_mode),
                preset: AiPreset::parse(&row.preset).unwrap_or_default(),
                model: row.model.clone(),
                api: Provider::new(
                    if row.api_url.is_empty() {
                        env.url.clone()
                    } else {
                        row.api_url.clone()
                    },
                    // The environment's key never goes to another address.
                    if row.api_key.is_empty() && follows_env {
                        env.key.clone()
                    } else {
                        row.api_key.clone()
                    },
                    if row.api_model.is_empty() {
                        env.model.clone()
                    } else {
                        row.api_model.clone()
                    },
                ),
            }
        })
        .collect();
    Ok(AiSetup {
        tools,
        autotag: autotag.unwrap_or(env.autotag),
        auto_translate: auto_translate.unwrap_or_else(|| env.translate_to.clone()),
    })
}

/// Saves one tool's setup (already cleaned). `api_key`: `None` keeps the
/// stored one.
pub async fn save_ai_tool(state: &AppState, tool: AiTool, update: &AiToolUpdate) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO ai_tools (tool, mode, preset, model, api_url, api_model, api_key) \
         VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, '')) \
         ON CONFLICT (tool) DO UPDATE SET mode = $2, preset = $3, model = $4, api_url = $5, \
         api_model = $6, api_key = COALESCE($7, ai_tools.api_key)",
    )
    .bind(tool.as_str())
    .bind(update.mode.as_str())
    .bind(update.preset.as_str())
    .bind(&update.model)
    .bind(&update.api_url)
    .bind(&update.api_model)
    .bind(&update.api_key)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Saves the settings all AI tools share (already cleaned), like [`save`].
pub async fn save_ai_general(state: &AppState, update: &AiGeneralUpdate) -> AppResult<()> {
    let env = &state.config.ai;
    sqlx::query(
        "INSERT INTO server_settings (id, ai_autotag, ai_auto_translate) VALUES (true, $1, $2) \
         ON CONFLICT (id) DO UPDATE SET ai_autotag = $1, ai_auto_translate = $2, \
         updated_at = now()",
    )
    .bind((update.autotag != env.autotag).then_some(update.autotag))
    .bind((update.auto_translate != env.translate_to).then_some(&update.auto_translate))
    .execute(&state.db)
    .await?;
    Ok(())
}
