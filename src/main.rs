use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand};
use serde::Deserialize;
use webtrans::html;
use webtrans::model::TranslationTable;
use webtrans::translator::{TranslationConfig, translate_table};

#[derive(Parser)]
#[command(
    version,
    about = "Translate HTML through a reviewable JSON table while preserving its source markup."
)]
struct Cli {
    /// Print every translation-table entry in stage logs (default: first 5).
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Extract translatable content into a JSON table. No network access.
    Extract {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Source language hint (otherwise the translation API detects it).
        #[arg(long)]
        from: Option<String>,
        /// Replace an existing output file.
        #[arg(long)]
        force: bool,
    },
    /// Translate a JSON table through an OpenAI-compatible chat completions API.
    Translate {
        table: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[command(flatten)]
        api: ApiArgs,
        #[command(flatten)]
        recovery: RecoveryArgs,
    },
    /// Apply a translated table to the exact original HTML. No network access.
    Apply {
        input: PathBuf,
        table: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Leave entries without translations unchanged instead of failing.
        #[arg(long)]
        allow_missing: bool,
        #[arg(long)]
        force: bool,
    },
    /// Extract, translate, and apply in one command, saving a resumable table.
    Run {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Checkpoint JSON path (default: output file with .translations.json extension).
        #[arg(long)]
        table: Option<PathBuf>,
        #[command(flatten)]
        api: ApiArgs,
        #[command(flatten)]
        recovery: RecoveryArgs,
    },
}

#[derive(Args)]
struct ApiArgs {
    /// JSON configuration file (automatically reads ./config.json when present).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Target language, e.g. zh-CN, en, ja.
    #[arg(long)]
    to: String,
    /// Optional source language hint.
    #[arg(long)]
    from: Option<String>,
    /// Model name supported by your provider.
    #[arg(long)]
    model: Option<String>,
    /// API base URL or full chat/completions endpoint.
    #[arg(long)]
    base_url: Option<String>,
    /// Name of the environment variable containing the API key (never the key itself).
    #[arg(long)]
    api_key_env: Option<String>,
    /// Maximum number of entries per request.
    #[arg(long, default_value_t = 24)]
    batch_size: usize,
    /// Maximum source/context/marker characters per batch (not a token limit).
    #[arg(long, default_value_t = 12000)]
    max_batch_chars: usize,
    /// Timeout in seconds for each API request.
    #[arg(long, default_value_t = 90)]
    timeout: u64,
    /// Maximum additional attempts for transient API failures.
    #[arg(long, default_value_t = 2)]
    retries: usize,
    /// Send response_format=json_object, if supported by the provider.
    #[arg(long)]
    json_mode: bool,
}

// Do not derive Debug: these types hold a secret supplied by the user.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApiFileConfig {
    #[serde(alias = "baseurl", alias = "BASE_URL")]
    base_url: Option<String>,
    #[serde(alias = "apikey", alias = "APIKEY", alias = "API_KEY")]
    api_key: Option<String>,
    model: Option<String>,
}

struct ApiSettings {
    base_url: String,
    api_key: String,
    model: String,
    config_path: Option<PathBuf>,
}

#[derive(Args)]
struct RecoveryArgs {
    /// Continue from an existing output table/checkpoint. Completed entries are skipped.
    #[arg(long, conflicts_with = "force")]
    resume: bool,
    /// Replace existing output files and start from the supplied source/table.
    #[arg(long)]
    force: bool,
}

fn main() {
    if let Err(error) = execute(Cli::parse()) {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

fn execute(cli: Cli) -> Result<()> {
    let verbose = cli.verbose;
    match cli.command {
        Command::Extract {
            input,
            output,
            from,
            force,
        } => {
            ensure_distinct(&output, &[&input])?;
            check_output(&output, force)?;
            let source = read_html(&input)?;
            let mut table = html::extract(&source)?;
            table.source_language = nonempty_language(from)?;
            write_table(&output, &table, force)?;
            log_table("extract / 提取完成", &table, None, verbose)?;
            eprintln!(
                "Extracted {} entries → {}",
                table.entries.len(),
                output.display()
            );
        }
        Command::Apply {
            input,
            table,
            output,
            allow_missing,
            force,
        } => {
            ensure_distinct(&output, &[&input, &table])?;
            check_output(&output, force)?;
            let source = read_html(&input)?;
            let translations = read_table(&table)?;
            let result = html::apply(&source, &translations, allow_missing)?;
            log_table("apply / 校验通过，准备回填", &translations, None, verbose)?;
            write_atomic(&output, result.as_bytes(), force)?;
            let completed = completed_count(&translations);
            eprintln!(
                "Applied {completed}/{} entries → {}",
                translations.entries.len(),
                output.display()
            );
        }
        Command::Translate {
            table,
            output,
            api,
            recovery,
        } => {
            ensure_distinct(&output, &[&table])?;
            let settings = load_api_settings(&api)?;
            protect_config(&settings, &[&output])?;
            let input_table = read_table(&table)?;
            let mut translations = prepare_table(input_table, &output, &recovery)?;
            translate_and_save(
                &mut translations,
                &output,
                &api,
                &settings,
                &recovery,
                verbose,
            )?;
            eprintln!(
                "Translated {} entries → {}",
                translations.entries.len(),
                output.display()
            );
        }
        Command::Run {
            input,
            output,
            table,
            api,
            recovery,
        } => {
            let table_path = table.unwrap_or_else(|| output.with_extension("translations.json"));
            ensure_distinct(&output, &[&input, &table_path])?;
            ensure_distinct(&table_path, &[&input])?;
            check_output(&output, recovery.force)?;
            let settings = load_api_settings(&api)?;
            protect_config(&settings, &[&output, &table_path])?;
            let source = read_html(&input)?;
            let mut extracted = html::extract(&source)?;
            extracted.source_language = nonempty_language(api.from.clone())?;
            log_table("extract / 提取完成", &extracted, None, verbose)?;
            let mut translations = prepare_table(extracted, &table_path, &recovery)?;
            translate_and_save(
                &mut translations,
                &table_path,
                &api,
                &settings,
                &recovery,
                verbose,
            )?;
            let result = html::apply(&source, &translations, false)?;
            eprintln!(
                "[apply / 回填] Source hash, entry ranges and placeholders verified; writing {} translated entries.",
                completed_count(&translations)
            );
            write_atomic(&output, result.as_bytes(), recovery.force)?;
            eprintln!("Translated HTML → {}", output.display());
            eprintln!("Reviewable table → {}", table_path.display());
        }
    }
    Ok(())
}

fn translate_and_save(
    table: &mut TranslationTable,
    output: &Path,
    api: &ApiArgs,
    settings: &ApiSettings,
    recovery: &RecoveryArgs,
    verbose: bool,
) -> Result<()> {
    let target = nonempty_language(Some(api.to.clone()))?.context("Target language is required")?;
    let source_hint = nonempty_language(api.from.clone())?;
    if let Some(hint) = &source_hint
        && let Some(existing) = &table.source_language
    {
        ensure!(
            existing == hint,
            "Source language differs from the table; use its original source language"
        );
    }
    let config = TranslationConfig {
        base_url: settings.base_url.clone(),
        api_key: settings.api_key.clone(),
        model: settings.model.clone(),
        target_language: target,
        source_language: source_hint.or_else(|| table.source_language.clone()),
        batch_size: api.batch_size,
        max_batch_chars: api.max_batch_chars,
        timeout_secs: api.timeout,
        retries: api.retries,
        json_mode: api.json_mode,
    };
    log_table("translate / 翻译前", table, None, verbose)?;
    let mut saved_ids: BTreeSet<String> = table
        .entries
        .iter()
        .filter(|(_, entry)| entry.translation.is_some())
        .map(|(id, _)| id.clone())
        .collect();
    let mut has_checkpoint = recovery.resume && output.try_exists()?;
    let result = translate_table(
        table,
        &config,
        |current, completed_batches, total_batches| {
            write_table(output, current, recovery.force || has_checkpoint)?;
            has_checkpoint = true;
            let batch_ids: BTreeSet<String> = current
                .entries
                .iter()
                .filter(|(id, entry)| entry.translation.is_some() && !saved_ids.contains(*id))
                .map(|(id, _)| id.clone())
                .collect();
            eprintln!(
                "Batch {completed_batches}/{total_batches}: {}/{} entries saved → {}",
                completed_count(current),
                current.entries.len(),
                output.display()
            );
            log_table(
                "batch-saved / 本批译文已校验并保存",
                current,
                Some(&batch_ids),
                verbose,
            )?;
            saved_ids.extend(batch_ids);
            Ok(())
        },
    );
    if let Err(error) = result {
        if has_checkpoint {
            eprintln!(
                "Completed work is saved in {}. Use --resume to continue.",
                output.display()
            );
        }
        return Err(error);
    }
    // Also persist empty tables or already completed input tables (no API batches).
    write_table(output, table, recovery.force || has_checkpoint)?;
    Ok(())
}

fn load_api_settings(api: &ApiArgs) -> Result<ApiSettings> {
    let path = api
        .config
        .as_deref()
        .unwrap_or_else(|| Path::new("config.json"));
    let (file, config_path) = match fs::read_to_string(path) {
        Ok(json) => {
            let file: ApiFileConfig = serde_json::from_str(&json).map_err(|error| {
                // serde errors may echo invalid field values; only expose location.
                anyhow::anyhow!(
                    "Invalid API config {} at line {}, column {}. Expected a JSON object with string fields base_url, api_key and model",
                    path.display(), error.line(), error.column()
                )
            })?;
            (file, Some(path.to_path_buf()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && api.config.is_none() => {
            (ApiFileConfig::default(), None)
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Cannot read API config {}", path.display()));
        }
    };
    // Explicit CLI options > file settings > environment > built-in defaults.
    let base_url = api
        .base_url
        .clone()
        .or(file.base_url)
        .or_else(|| std::env::var("WEBTRANS_BASE_URL").ok())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_owned());
    let model = api
        .model
        .clone()
        .or(file.model)
        .or_else(|| std::env::var("WEBTRANS_MODEL").ok())
        .context(
            "Model is required: fill model in config.json, use --model, or set WEBTRANS_MODEL",
        )?;
    let api_key = if let Some(name) = &api.api_key_env {
        std::env::var(name).with_context(|| {
            format!("Set the {name} environment variable selected by --api-key-env")
        })?
    } else {
        file.api_key
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .context("API key is required: fill api_key in config.json or set OPENAI_API_KEY")?
    };
    ensure!(
        !base_url.trim().is_empty(),
        "API base URL is empty; fill base_url in config.json or use --base-url"
    );
    ensure!(
        !model.trim().is_empty(),
        "Model is empty; fill model in config.json or use --model"
    );
    ensure!(
        !api_key.trim().is_empty(),
        "API key is empty; fill api_key in config.json or use --api-key-env"
    );
    if let Some(path) = &config_path {
        eprintln!(
            "[config / 配置] Loaded {}; API key is hidden.",
            path.display()
        );
    }
    Ok(ApiSettings {
        base_url,
        api_key,
        model,
        config_path,
    })
}

fn protect_config(settings: &ApiSettings, outputs: &[&Path]) -> Result<()> {
    if let Some(path) = &settings.config_path {
        for output in outputs {
            ensure_distinct(output, &[path])
                .context("Output must not overwrite the API config file")?;
        }
    }
    Ok(())
}

fn prepare_table(
    original: TranslationTable,
    output: &Path,
    recovery: &RecoveryArgs,
) -> Result<TranslationTable> {
    if recovery.resume && output.try_exists()? {
        let resumed = read_table(output)?;
        ensure_same_source_table(&original, &resumed)?;
        eprintln!(
            "Resuming {}/{} completed entries from {}",
            completed_count(&resumed),
            resumed.entries.len(),
            output.display()
        );
        return Ok(resumed);
    }
    check_output(output, recovery.force)?;
    Ok(original)
}

fn ensure_same_source_table(original: &TranslationTable, resumed: &TranslationTable) -> Result<()> {
    let mut expected = original.clone();
    let mut actual = resumed.clone();
    expected.target_language = None;
    actual.target_language = None;
    // A source hint can have been supplied at translation time.
    if expected.source_language.is_none() {
        expected.source_language = actual.source_language.clone();
    }
    for entry in expected
        .entries
        .values_mut()
        .chain(actual.entries.values_mut())
    {
        entry.translation = None;
    }
    ensure!(
        expected == actual,
        "Checkpoint belongs to different HTML or an altered extraction table"
    );
    Ok(())
}

fn nonempty_language(value: Option<String>) -> Result<Option<String>> {
    value
        .map(|value| {
            ensure!(!value.trim().is_empty(), "Language must not be empty");
            Ok(value.trim().to_owned())
        })
        .transpose()
}

fn completed_count(table: &TranslationTable) -> usize {
    table
        .entries
        .values()
        .filter(|entry| entry.translation.is_some())
        .count()
}

/// Print the real schema to stderr; the persisted JSON remains the full table.
fn log_table(
    stage: &str,
    table: &TranslationTable,
    only_ids: Option<&BTreeSet<String>>,
    verbose: bool,
) -> Result<()> {
    let eligible: Vec<_> = table
        .entries
        .iter()
        .filter(|(id, _)| only_ids.is_none_or(|ids| ids.contains(*id)))
        .collect();
    let shown = if verbose {
        eligible.len()
    } else {
        eligible.len().min(5)
    };
    let entries: BTreeMap<_, _> = eligible[..shown].iter().copied().collect();
    let preview = serde_json::json!({
        "version": table.version,
        "source_sha256": table.source_sha256,
        "source_language": table.source_language,
        "target_language": table.target_language,
        "entries": entries,
    });
    eprintln!(
        "\n[{stage}] Hash table: {} entries total, {}/{} entries shown below; {}/{} translated.",
        table.entries.len(),
        shown,
        eligible.len(),
        completed_count(table),
        table.entries.len()
    );
    eprintln!("{}", serde_json::to_string_pretty(&preview)?);
    if shown < eligible.len() {
        eprintln!(
            "... {} entries omitted from this log preview. Use --verbose to print all entries; saved JSON is complete.",
            eligible.len() - shown
        );
    }
    Ok(())
}

fn read_html(path: &Path) -> Result<String> {
    fs::read_to_string(path)
        .with_context(|| format!("Cannot read UTF-8 HTML from {}", path.display()))
}

fn read_table(path: &Path) -> Result<TranslationTable> {
    let json = fs::read_to_string(path)
        .with_context(|| format!("Cannot read translation table {}", path.display()))?;
    let table: TranslationTable = serde_json::from_str(&json)
        .with_context(|| format!("Invalid translation table {}", path.display()))?;
    ensure!(
        table.version == 1,
        "Unsupported translation table version: {}",
        table.version
    );
    Ok(table)
}

fn destination(path: &Path) -> Result<PathBuf> {
    if path.try_exists()? {
        return fs::canonicalize(path)
            .with_context(|| format!("Cannot resolve {}", path.display()));
    }
    let parent = output_parent(path);
    let filename = path.file_name().context("Output must be a file path")?;
    Ok(fs::canonicalize(parent)
        .with_context(|| format!("Output directory does not exist: {}", parent.display()))?
        .join(filename))
}

fn ensure_distinct(output: &Path, inputs: &[&Path]) -> Result<()> {
    let output = destination(output)?;
    for input in inputs {
        ensure!(
            output != destination(input)?,
            "Output must be different from every input and checkpoint file"
        );
    }
    Ok(())
}

fn check_output(path: &Path, overwrite: bool) -> Result<()> {
    // symlink_metadata also catches broken symlinks; they are existing outputs.
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                !metadata.is_dir(),
                "Output is a directory: {}",
                path.display()
            );
            ensure!(
                overwrite,
                "Output already exists: {} (use --force to replace or --resume to continue translation)",
                path.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Cannot inspect output path"),
    }
    ensure!(
        output_parent(path).is_dir(),
        "Output directory does not exist: {}",
        output_parent(path).display()
    );
    Ok(())
}

fn output_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn write_table(path: &Path, table: &TranslationTable, overwrite: bool) -> Result<()> {
    let mut json = serde_json::to_vec_pretty(table)?;
    json.push(b'\n');
    write_atomic(path, &json, overwrite)
}

fn write_atomic(path: &Path, bytes: &[u8], overwrite: bool) -> Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(output_parent(path))
        .with_context(|| format!("Cannot create temporary output near {}", path.display()))?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    let result = if overwrite {
        temporary.persist(path)
    } else {
        temporary.persist_noclobber(path)
    };
    if let Err(error) = result {
        bail!("Cannot save {}: {}", path.display(), error.error);
    }
    Ok(())
}
