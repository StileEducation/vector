use std::collections::HashMap;
use std::sync::Arc;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read},
    path::PathBuf,
};

use codecs::MetricTagValues;
use lookup::lookup_v2::{parse_value_path, ValuePath};
use lookup::{metadata_path, owned_value_path, path, OwnedTargetPath, PathPrefix};
use snafu::{ResultExt, Snafu};
use value::kind::merge::{CollisionStrategy, Strategy};
use value::kind::Collection;
use value::Kind;
use vector_common::TimeZone;
use vector_config::configurable_component;
use vector_core::compile_vrl;
use vector_core::config::LogNamespace;
use vector_core::schema::Definition;
use vector_vrl_functions::set_semantic_meaning::MeaningList;
use vrl::prelude::state::TypeState;
use vrl::{
    diagnostic::{Formatter, Note},
    prelude::{DiagnosticMessage, ExpressionError},
    CompileConfig, Program, Runtime, Terminate, VrlRuntime,
};

use crate::config::OutputId;
use crate::{
    config::{
        log_schema, ComponentKey, DataType, Input, TransformConfig, TransformContext,
        TransformOutput,
    },
    event::{Event, TargetEvents, VrlTarget},
    internal_events::{RemapMappingAbort, RemapMappingError},
    schema,
    transforms::{SyncTransform, Transform, TransformOutputsBuf},
    Result,
};

const DROPPED: &str = "dropped";

/// Configuration for the `remap` transform.
#[configurable_component(transform(
    "remap",
    "Modify your observability data as it passes through your topology using Vector Remap Language (VRL)."
))]
#[derive(Clone, Debug, Derivative)]
#[serde(deny_unknown_fields)]
#[derivative(Default)]
pub struct RemapConfig {
    /// The [Vector Remap Language][vrl] (VRL) program to execute for each event.
    ///
    /// Required if `file` is missing.
    ///
    /// [vrl]: https://vector.dev/docs/reference/vrl
    #[configurable(metadata(
        docs::examples = ". = parse_json!(.message)\n.new_field = \"new value\"\n.status = to_int!(.status)\n.duration = parse_duration!(.duration, \"s\")\n.new_name = del(.old_name)",
        docs::syntax_override = "remap_program"
    ))]
    pub source: Option<String>,

    /// File path to the [Vector Remap Language][vrl] (VRL) program to execute for each event.
    ///
    /// If a relative path is provided, its root is the current working directory.
    ///
    /// Required if `source` is missing.
    ///
    /// [vrl]: https://vector.dev/docs/reference/vrl
    #[configurable(metadata(docs::examples = "./my/program.vrl"))]
    pub file: Option<PathBuf>,

    /// When set to `single`, metric tag values are exposed as single strings, the
    /// same as they were before this config option. Tags with multiple values show the last assigned value, and null values
    /// are ignored.
    ///
    /// When set to `full`, all metric tags are exposed as arrays of either string or null
    /// values.
    #[serde(default)]
    pub metric_tag_values: MetricTagValues,

    /// The name of the timezone to apply to timestamp conversions that do not contain an explicit
    /// time zone.
    ///
    /// This overrides the [global `timezone`][global_timezone] option. The time zone name may be
    /// any name in the [TZ database][tz_database], or `local` to indicate system local time.
    ///
    /// [global_timezone]: https://vector.dev/docs/reference/configuration//global-options#timezone
    /// [tz_database]: https://en.wikipedia.org/wiki/List_of_tz_database_time_zones
    #[serde(default)]
    #[configurable(metadata(docs::advanced))]
    pub timezone: Option<TimeZone>,

    /// Drops any event that encounters an error during processing.
    ///
    /// Normally, if a VRL program encounters an error when processing an event, the original,
    /// unmodified event is sent downstream. In some cases, you may not want to send the event
    /// any further, such as if certain transformation or enrichment is strictly required. Setting
    /// `drop_on_error` to `true` allows you to ensure these events do not get processed any
    /// further.
    ///
    /// Additionally, dropped events can potentially be diverted to a specially named output for
    /// further logging and analysis by setting `reroute_dropped`.
    #[serde(default = "crate::serde::default_false")]
    pub drop_on_error: bool,

    /// Drops any event that is manually aborted during processing.
    ///
    /// Normally, if a VRL program is manually aborted (using [`abort`][vrl_docs_abort]) when
    /// processing an event, the original, unmodified event is sent downstream. In some cases,
    /// you may not wish to send the event any further, such as if certain transformation or
    /// enrichment is strictly required. Setting `drop_on_abort` to `true` allows you to ensure
    /// these events do not get processed any further.
    ///
    /// Additionally, dropped events can potentially be diverted to a specially-named output for
    /// further logging and analysis by setting `reroute_dropped`.
    ///
    /// [vrl_docs_abort]: https://vector.dev/docs/reference/vrl/expressions/#abort
    #[serde(default = "crate::serde::default_true")]
    pub drop_on_abort: bool,

    /// Reroutes dropped events to a named output instead of halting processing on them.
    ///
    /// When using `drop_on_error` or `drop_on_abort`, events that are "dropped" are processed no
    /// further. In some cases, it may be desirable to keep the events around for further analysis,
    /// debugging, or retrying.
    ///
    /// In these cases, `reroute_dropped` can be set to `true` which forwards the original event
    /// to a specially-named output, `dropped`. The original event is annotated with additional
    /// fields describing why the event was dropped.
    #[serde(default = "crate::serde::default_false")]
    pub reroute_dropped: bool,

    #[configurable(derived, metadata(docs::hidden))]
    #[serde(default)]
    pub runtime: VrlRuntime,
}

impl RemapConfig {
    fn compile_vrl_program(
        &self,
        enrichment_tables: enrichment::TableRegistry,
        merged_schema_definition: schema::Definition,
    ) -> Result<(
        vrl::Program,
        String,
        Vec<Box<dyn vrl::Function>>,
        CompileConfig,
    )> {
        let source = match (&self.source, &self.file) {
            (Some(source), None) => source.to_owned(),
            (None, Some(path)) => {
                let mut buffer = String::new();

                File::open(path)
                    .with_context(|_| FileOpenFailedSnafu { path })?
                    .read_to_string(&mut buffer)
                    .with_context(|_| FileReadFailedSnafu { path })?;

                buffer
            }
            _ => return Err(Box::new(BuildError::SourceAndOrFile)),
        };

        let mut functions = vrl_stdlib::all();
        functions.append(&mut enrichment::vrl_functions());
        functions.append(&mut vector_vrl_functions::all());

        let state = TypeState {
            local: Default::default(),
            external: vrl::state::ExternalEnv::new_with_kind(
                merged_schema_definition.event_kind().clone(),
                merged_schema_definition.metadata_kind().clone(),
            ),
        };
        let mut config = CompileConfig::default();

        config.set_custom(enrichment_tables);
        config.set_custom(MeaningList::default());

        compile_vrl(&source, &functions, &state, config)
            .map_err(|diagnostics| {
                Formatter::new(&source, diagnostics)
                    .colored()
                    .to_string()
                    .into()
            })
            .map(|result| {
                (
                    result.program,
                    Formatter::new(&source, result.warnings).to_string(),
                    functions,
                    result.config,
                )
            })
    }
}

impl_generate_config_from_default!(RemapConfig);

#[async_trait::async_trait]
#[typetag::serde(name = "remap")]
impl TransformConfig for RemapConfig {
    async fn build(&self, context: &TransformContext) -> Result<Transform> {
        let (transform, warnings) = match self.runtime {
            VrlRuntime::Ast => {
                let (remap, warnings) = Remap::new_ast(self.clone(), context)?;
                (Transform::synchronous(remap), warnings)
            }
        };

        // TODO: We could improve on this by adding support for non-fatal error
        // messages in the topology. This would make the topology responsible
        // for printing warnings (including potentially emitting metrics),
        // instead of individual transforms.
        if !warnings.is_empty() {
            warn!(message = "VRL compilation warning.", %warnings);
        }

        Ok(transform)
    }

    fn input(&self) -> Input {
        Input::all()
    }

    fn outputs(
        &self,
        enrichment_tables: enrichment::TableRegistry,
        input_definitions: &[(OutputId, schema::Definition)],
        _: LogNamespace,
    ) -> Vec<TransformOutput> {
        let merged_definition: Definition = input_definitions
            .iter()
            .map(|(_output, definition)| definition.clone())
            .reduce(Definition::merge)
            .unwrap_or_else(Definition::any);

        // We need to compile the VRL program in order to know the schema definition output of this
        // transform. We ignore any compilation errors, as those are caught by the transform build
        // step.
        let compiled = self
            .compile_vrl_program(enrichment_tables, merged_definition)
            .map(|(program, _, _, external_context)| {
                (
                    program.final_type_state(),
                    external_context
                        .get_custom::<MeaningList>()
                        .cloned()
                        .expect("context exists")
                        .0,
                )
            })
            .map_err(|_| ());

        let mut dropped_definitions = HashMap::new();
        let mut default_definitions = HashMap::new();

        for (output_id, input_definition) in input_definitions {
            let default_definition = compiled
                .clone()
                .map(|(state, meaning)| {
                    let mut new_type_def = Definition::new(
                        state.external.target_kind().clone(),
                        state.external.metadata_kind().clone(),
                        input_definition.log_namespaces().clone(),
                    );

                    for (id, path) in input_definition.meanings() {
                        // Attempt to copy over the meanings from the input definition.
                        // The function will fail if the meaning that now points to a field that no longer exists,
                        // this is fine since we will no longer want that meaning in the output definition.
                        let _ = new_type_def.try_with_meaning(path.clone(), id);
                    }

                    // Apply any semantic meanings set in the VRL program
                    for (id, path) in meaning {
                        // currently only event paths are supported
                        new_type_def = new_type_def.with_meaning(OwnedTargetPath::event(path), &id);
                    }
                    new_type_def
                })
                .unwrap_or_else(|_| {
                    Definition::new_with_default_metadata(
                        // The program failed to compile, so it can "never" return a value
                        Kind::never(),
                        input_definition.log_namespaces().clone(),
                    )
                });

            // When a message is dropped and re-routed, we keep the original event, but also annotate
            // it with additional metadata.
            let mut dropped_definition = Definition::new_with_default_metadata(
                Kind::never(),
                input_definition.log_namespaces().clone(),
            );

            if input_definition
                .log_namespaces()
                .contains(&LogNamespace::Legacy)
            {
                dropped_definition =
                    dropped_definition.merge(input_definition.clone().with_event_field(
                        &parse_value_path(log_schema().metadata_key()).expect("valid metadata key"),
                        Kind::object(BTreeMap::from([
                            ("reason".into(), Kind::bytes()),
                            ("message".into(), Kind::bytes()),
                            ("component_id".into(), Kind::bytes()),
                            ("component_type".into(), Kind::bytes()),
                            ("component_kind".into(), Kind::bytes()),
                        ])),
                        Some("metadata"),
                    ));
            }

            if input_definition
                .log_namespaces()
                .contains(&LogNamespace::Vector)
            {
                dropped_definition = dropped_definition.merge(
                    input_definition
                        .clone()
                        .with_metadata_field(&owned_value_path!("reason"), Kind::bytes(), None)
                        .with_metadata_field(&owned_value_path!("message"), Kind::bytes(), None)
                        .with_metadata_field(
                            &owned_value_path!("component_id"),
                            Kind::bytes(),
                            None,
                        )
                        .with_metadata_field(
                            &owned_value_path!("component_type"),
                            Kind::bytes(),
                            None,
                        )
                        .with_metadata_field(
                            &owned_value_path!("component_kind"),
                            Kind::bytes(),
                            None,
                        ),
                );
            }

            default_definitions.insert(
                output_id.clone(),
                move_field_definitions_into_message(merge_array_definitions(default_definition)),
            );
            dropped_definitions.insert(
                output_id.clone(),
                move_field_definitions_into_message(merge_array_definitions(dropped_definition)),
            );
        }

        let default_output = TransformOutput::new(DataType::all(), default_definitions);

        if self.reroute_dropped {
            vec![
                default_output,
                TransformOutput::new(DataType::all(), dropped_definitions).with_port(DROPPED),
            ]
        } else {
            vec![default_output]
        }
    }

    fn enable_concurrency(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone)]
pub struct Remap<Runner>
where
    Runner: VrlRunner,
{
    component_key: Option<ComponentKey>,
    program: Program,
    timezone: TimeZone,
    drop_on_error: bool,
    drop_on_abort: bool,
    reroute_dropped: bool,
    default_schema_definition: Arc<schema::Definition>,
    dropped_schema_definition: Arc<schema::Definition>,
    runner: Runner,
    metric_tag_values: MetricTagValues,
}

pub trait VrlRunner {
    fn run(
        &mut self,
        target: &mut VrlTarget,
        program: &Program,
        timezone: &TimeZone,
    ) -> std::result::Result<value::Value, Terminate>;
}

#[derive(Debug)]
pub struct AstRunner {
    pub runtime: Runtime,
}

impl Clone for AstRunner {
    fn clone(&self) -> Self {
        Self {
            runtime: Runtime::default(),
        }
    }
}

impl VrlRunner for AstRunner {
    fn run(
        &mut self,
        target: &mut VrlTarget,
        program: &Program,
        timezone: &TimeZone,
    ) -> std::result::Result<value::Value, Terminate> {
        let result = self.runtime.resolve(target, program, timezone);
        self.runtime.clear();
        result
    }
}

impl Remap<AstRunner> {
    pub fn new_ast(
        config: RemapConfig,
        context: &TransformContext,
    ) -> crate::Result<(Self, String)> {
        let (program, warnings, _, _) = config.compile_vrl_program(
            context.enrichment_tables.clone(),
            context.merged_schema_definition.clone(),
        )?;

        let runtime = Runtime::default();
        let runner = AstRunner { runtime };

        Self::new(config, context, program, runner).map(|remap| (remap, warnings))
    }
}

impl<Runner> Remap<Runner>
where
    Runner: VrlRunner,
{
    fn new(
        config: RemapConfig,
        context: &TransformContext,
        program: Program,
        runner: Runner,
    ) -> crate::Result<Self> {
        let default_schema_definition = context
            .schema_definitions
            .get(&None)
            .expect("default schema required")
            // TODO we can now have multiple possible definitions.
            // This is going to need to be updated to store these possible definitions and then
            // choose the correct one based on the input the event has come from.
            .iter()
            .map(|(_output, definition)| definition.clone())
            .next()
            .unwrap_or_else(Definition::any);

        let dropped_schema_definition = context
            .schema_definitions
            .get(&Some(DROPPED.to_owned()))
            .or_else(|| context.schema_definitions.get(&None))
            .expect("dropped schema required")
            .iter()
            .map(|(_output, definition)| definition.clone())
            .next()
            .unwrap_or_else(Definition::any);

        Ok(Remap {
            component_key: context.key.clone(),
            program,
            timezone: config
                .timezone
                .unwrap_or_else(|| context.globals.timezone()),
            drop_on_error: config.drop_on_error,
            drop_on_abort: config.drop_on_abort,
            reroute_dropped: config.reroute_dropped,
            default_schema_definition: Arc::new(default_schema_definition),
            dropped_schema_definition: Arc::new(dropped_schema_definition),
            runner,
            metric_tag_values: config.metric_tag_values,
        })
    }

    #[cfg(test)]
    const fn runner(&self) -> &Runner {
        &self.runner
    }

    fn dropped_data(&self, reason: &str, error: ExpressionError) -> serde_json::Value {
        let message = error
            .notes()
            .iter()
            .filter(|note| matches!(note, Note::UserErrorMessage(_)))
            .last()
            .map(|note| note.to_string())
            .unwrap_or_else(|| error.to_string());
        serde_json::json!({
                "reason": reason,
                "message": message,
                "component_id": self.component_key,
                "component_type": "remap",
                "component_kind": "transform",
        })
    }

    fn annotate_dropped(&self, event: &mut Event, reason: &str, error: ExpressionError) {
        match event {
            Event::Log(ref mut log) => match log.namespace() {
                LogNamespace::Legacy => {
                    log.insert(
                        (
                            PathPrefix::Event,
                            log_schema().metadata_key().concat(path!("dropped")),
                        ),
                        self.dropped_data(reason, error),
                    );
                }
                LogNamespace::Vector => {
                    log.insert(
                        metadata_path!("vector", "dropped"),
                        self.dropped_data(reason, error),
                    );
                }
            },
            Event::Metric(ref mut metric) => {
                let m = log_schema().metadata_key();
                metric.replace_tag(format!("{}.dropped.reason", m), reason.into());
                metric.replace_tag(
                    format!("{}.dropped.component_id", m),
                    self.component_key
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(String::new),
                );
                metric.replace_tag(format!("{}.dropped.component_type", m), "remap".into());
                metric.replace_tag(format!("{}.dropped.component_kind", m), "transform".into());
            }
            Event::Trace(ref mut trace) => {
                trace.insert(
                    log_schema().metadata_key(),
                    self.dropped_data(reason, error),
                );
            }
        }
    }

    fn run_vrl(&mut self, target: &mut VrlTarget) -> std::result::Result<value::Value, Terminate> {
        self.runner.run(target, &self.program, &self.timezone)
    }
}

impl<Runner> SyncTransform for Remap<Runner>
where
    Runner: VrlRunner + Clone + Send + Sync,
{
    fn transform(&mut self, event: Event, output: &mut TransformOutputsBuf) {
        // If a program can fail or abort at runtime and we know that we will still need to forward
        // the event in that case (either to the main output or `dropped`, depending on the
        // config), we need to clone the original event and keep it around, to allow us to discard
        // any mutations made to the event while the VRL program runs, before it failed or aborted.
        //
        // The `drop_on_{error, abort}` transform config allows operators to remove events from the
        // main output if they're failed or aborted, in which case we can skip the cloning, since
        // any mutations made by VRL will be ignored regardless. If they hav configured
        // `reroute_dropped`, however, we still need to do the clone to ensure that we can forward
        // the event to the `dropped` output.
        let forward_on_error = !self.drop_on_error || self.reroute_dropped;
        let forward_on_abort = !self.drop_on_abort || self.reroute_dropped;
        let original_event = if (self.program.info().fallible && forward_on_error)
            || (self.program.info().abortable && forward_on_abort)
        {
            Some(event.clone())
        } else {
            None
        };

        let mut target = VrlTarget::new(
            event,
            self.program.info(),
            match self.metric_tag_values {
                MetricTagValues::Single => false,
                MetricTagValues::Full => true,
            },
        );
        let result = self.run_vrl(&mut target);

        match result {
            Ok(_) => match target.into_events() {
                TargetEvents::One(event) => {
                    push_default(event, output, &self.default_schema_definition)
                }
                TargetEvents::Logs(events) => events
                    .for_each(|event| push_default(event, output, &self.default_schema_definition)),
                TargetEvents::Traces(events) => events
                    .for_each(|event| push_default(event, output, &self.default_schema_definition)),
            },
            Err(reason) => {
                let (reason, error, drop) = match reason {
                    Terminate::Abort(error) => {
                        emit!(RemapMappingAbort {
                            event_dropped: self.drop_on_abort,
                        });

                        ("abort", error, self.drop_on_abort)
                    }
                    Terminate::Error(error) => {
                        emit!(RemapMappingError {
                            error: error.to_string(),
                            event_dropped: self.drop_on_error,
                        });

                        ("error", error, self.drop_on_error)
                    }
                };

                if let Some(mut event) = original_event {
                    if !drop {
                        push_default(event, output, &self.default_schema_definition);
                    } else if self.reroute_dropped {
                        self.annotate_dropped(&mut event, reason, error);
                        push_dropped(event, output, &self.dropped_schema_definition);
                    }
                } else if !drop || self.reroute_dropped {
                    // We shouldn't be able to get here: the original event should have been
                    // cloned if the program could error and we didn't want to drop it
                    warn!(
                        "Unexpected VRL error encountered: event has been dropped. program_is_falible={}, program_is_abortable={}, drop={}, reroute_dropped={}",
                        self.program.info().fallible,
                        self.program.info().abortable,
                        drop,
                        self.reroute_dropped
                    );
                }
            }
        }
    }
}

#[inline]
fn push_default(
    mut event: Event,
    output: &mut TransformOutputsBuf,
    schema_definition: &Arc<schema::Definition>,
) {
    event
        .metadata_mut()
        .set_schema_definition(schema_definition);

    output.push(event)
}

#[inline]
fn push_dropped(
    mut event: Event,
    output: &mut TransformOutputsBuf,
    schema_definition: &Arc<schema::Definition>,
) {
    event
        .metadata_mut()
        .set_schema_definition(schema_definition);

    output.push_named(DROPPED, event)
}

/// If the VRL returns a value that is not an array (see [`merge_array_definitions`]),
/// or an object, that data is moved into the `message` field.
fn move_field_definitions_into_message(mut definition: schema::Definition) -> schema::Definition {
    let mut message = definition.event_kind().clone();
    message.remove_object();
    message.remove_array();

    if !message.is_never() {
        // We need to add the given message type to a field called `message`
        // in the event.
        let message = Kind::object(Collection::from(BTreeMap::from([(
            log_schema().message_key().into(),
            message,
        )])));

        definition.event_kind_mut().remove_bytes();
        definition.event_kind_mut().remove_integer();
        definition.event_kind_mut().remove_float();
        definition.event_kind_mut().remove_boolean();
        definition.event_kind_mut().remove_timestamp();
        definition.event_kind_mut().remove_regex();
        definition.event_kind_mut().remove_null();

        *definition.event_kind_mut() = definition.event_kind().union(message);
    }

    definition
}

/// If the transform returns an array, the elements of this array will be separated
/// out into it's individual elements and passed downstream.
///
/// The potential types that the transform can output are any of the arrays
/// elements or any non-array elements that are within the definition. All these
/// definitions need to be merged together.
fn merge_array_definitions(mut definition: schema::Definition) -> schema::Definition {
    if let Some(array) = definition.event_kind().as_array() {
        let array_kinds = array.reduced_kind();

        let kind = definition.event_kind_mut();
        kind.remove_array();
        kind.merge(
            array_kinds,
            Strategy {
                collisions: CollisionStrategy::Union,
            },
        );
    }

    definition
}

#[derive(Debug, Snafu)]
pub enum BuildError {
    #[snafu(display("must provide exactly one of `source` or `file` configuration"))]
    SourceAndOrFile,

    #[snafu(display("Could not open vrl program {:?}: {}", path, source))]
    FileOpenFailed { path: PathBuf, source: io::Error },
    #[snafu(display("Could not read vrl program {:?}: {}", path, source))]
    FileReadFailed { path: PathBuf, source: io::Error },
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use indoc::{formatdoc, indoc};
    use value::{
        btreemap,
        kind::{Collection, Index},
    };
    use vector_core::{config::GlobalOptions, event::EventMetadata, metric_tags};

    use super::*;
    use crate::{
        config::{build_unit_tests, ConfigBuilder},
        event::{
            metric::{MetricKind, MetricValue},
            LogEvent, Metric, Value,
        },
        schema,
        test_util::components::{
            assert_transform_compliance, init_test, COMPONENT_MULTIPLE_OUTPUTS_TESTS,
        },
        transforms::test::create_topology,
        transforms::OutputBuffer,
    };
    use chrono::DateTime;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    fn test_default_schema_definition() -> schema::Definition {
        schema::Definition::empty_legacy_namespace().with_event_field(
            &owned_value_path!("a default field"),
            Kind::integer().or_bytes(),
            Some("default"),
        )
    }

    fn test_dropped_schema_definition() -> schema::Definition {
        schema::Definition::empty_legacy_namespace().with_event_field(
            &owned_value_path!("a dropped field"),
            Kind::boolean().or_null(),
            Some("dropped"),
        )
    }

    fn remap(config: RemapConfig) -> Result<Remap<AstRunner>> {
        let schema_definitions = HashMap::from([
            (
                None,
                [("source".into(), test_default_schema_definition())].into(),
            ),
            (
                Some(DROPPED.to_owned()),
                [("source".into(), test_dropped_schema_definition())].into(),
            ),
        ]);

        Remap::new_ast(config, &TransformContext::new_test(schema_definitions))
            .map(|(remap, _)| remap)
    }

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<RemapConfig>();
    }

    #[test]
    fn config_missing_source_and_file() {
        let config = RemapConfig {
            source: None,
            file: None,
            ..Default::default()
        };

        let err = remap(config).unwrap_err().to_string();
        assert_eq!(
            &err,
            "must provide exactly one of `source` or `file` configuration"
        )
    }

    #[test]
    fn config_both_source_and_file() {
        let config = RemapConfig {
            source: Some("".to_owned()),
            file: Some("".into()),
            ..Default::default()
        };

        let err = remap(config).unwrap_err().to_string();
        assert_eq!(
            &err,
            "must provide exactly one of `source` or `file` configuration"
        )
    }

    fn get_field_string(event: &Event, field: &str) -> String {
        event
            .as_log()
            .get(field)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn check_remap_doesnt_share_state_between_events() {
        let conf = RemapConfig {
            source: Some(".foo = .sentinel".to_string()),
            file: None,
            drop_on_error: true,
            drop_on_abort: false,
            ..Default::default()
        };
        let mut tform = remap(conf).unwrap();
        assert!(tform.runner().runtime.is_empty());

        let event1 = {
            let mut event1 = LogEvent::from("event1");
            event1.insert("sentinel", "bar");
            Event::from(event1)
        };
        let result1 = transform_one(&mut tform, event1).unwrap();
        assert_eq!(get_field_string(&result1, "message"), "event1");
        assert_eq!(get_field_string(&result1, "foo"), "bar");
        assert_eq!(
            result1.metadata().schema_definition(),
            &test_default_schema_definition()
        );
        assert!(tform.runner().runtime.is_empty());

        let event2 = {
            let event2 = LogEvent::from("event2");
            Event::from(event2)
        };
        let result2 = transform_one(&mut tform, event2).unwrap();
        assert_eq!(get_field_string(&result2, "message"), "event2");
        assert_eq!(result2.as_log().get("foo"), Some(&Value::Null));
        assert_eq!(
            result2.metadata().schema_definition(),
            &test_default_schema_definition()
        );
        assert!(tform.runner().runtime.is_empty());
    }

    #[test]
    fn check_remap_adds() {
        let event = {
            let mut event = LogEvent::from("augment me");
            event.insert("copy_from", "buz");
            Event::from(event)
        };

        let conf = RemapConfig {
            source: Some(
                r#"  .foo = "bar"
  .bar = "baz"
  .copy = .copy_from
"#
                .to_string(),
            ),
            file: None,
            drop_on_error: true,
            drop_on_abort: false,
            ..Default::default()
        };
        let mut tform = remap(conf).unwrap();
        let result = transform_one(&mut tform, event).unwrap();
        assert_eq!(get_field_string(&result, "message"), "augment me");
        assert_eq!(get_field_string(&result, "copy_from"), "buz");
        assert_eq!(get_field_string(&result, "foo"), "bar");
        assert_eq!(get_field_string(&result, "bar"), "baz");
        assert_eq!(get_field_string(&result, "copy"), "buz");

        assert_eq!(
            result.metadata().schema_definition(),
            &test_default_schema_definition()
        );
    }

    #[test]
    fn check_remap_emits_multiple() {
        let event = {
            let mut event = LogEvent::from("augment me");
            event.insert(
                "events",
                vec![btreemap!("message" => "foo"), btreemap!("message" => "bar")],
            );
            Event::from(event)
        };

        let conf = RemapConfig {
            source: Some(
                indoc! {r#"
                . = .events
            "#}
                .to_owned(),
            ),
            file: None,
            drop_on_error: true,
            drop_on_abort: false,
            ..Default::default()
        };
        let mut tform = remap(conf).unwrap();

        let out = collect_outputs(&mut tform, event);
        assert_eq!(2, out.primary.len());
        let mut result = out.primary.into_events();

        let r = result.next().unwrap();
        assert_eq!(get_field_string(&r, "message"), "foo");
        assert_eq!(
            r.metadata().schema_definition(),
            &test_default_schema_definition()
        );
        let r = result.next().unwrap();
        assert_eq!(get_field_string(&r, "message"), "bar");

        assert_eq!(
            r.metadata().schema_definition(),
            &test_default_schema_definition()
        );
    }

    #[test]
    fn check_remap_error() {
        let event = {
            let mut event = Event::Log(LogEvent::from("augment me"));
            event.as_mut_log().insert("bar", "is a string");
            event
        };

        let conf = RemapConfig {
            source: Some(formatdoc! {r#"
                .foo = "foo"
                .not_an_int = int!(.bar)
                .baz = 12
            "#}),
            file: None,
            drop_on_error: false,
            drop_on_abort: false,
            ..Default::default()
        };
        let mut tform = remap(conf).unwrap();

        let event = transform_one(&mut tform, event).unwrap();

        assert_eq!(event.as_log().get("bar"), Some(&Value::from("is a string")));
        assert!(event.as_log().get("foo").is_none());
        assert!(event.as_log().get("baz").is_none());
    }

    #[test]
    fn check_remap_error_drop() {
        let event = {
            let mut event = Event::Log(LogEvent::from("augment me"));
            event.as_mut_log().insert("bar", "is a string");
            event
        };

        let conf = RemapConfig {
            source: Some(formatdoc! {r#"
                .foo = "foo"
                .not_an_int = int!(.bar)
                .baz = 12
            "#}),
            file: None,
            drop_on_error: true,
            drop_on_abort: false,
            ..Default::default()
        };
        let mut tform = remap(conf).unwrap();

        assert!(transform_one(&mut tform, event).is_none())
    }

    #[test]
    fn check_remap_error_infallible() {
        let event = {
            let mut event = Event::Log(LogEvent::from("augment me"));
            event.as_mut_log().insert("bar", "is a string");
            event
        };

        let conf = RemapConfig {
            source: Some(formatdoc! {r#"
                .foo = "foo"
                .baz = 12
            "#}),
            file: None,
            drop_on_error: false,
            drop_on_abort: false,
            ..Default::default()
        };
        let mut tform = remap(conf).unwrap();

        let event = transform_one(&mut tform, event).unwrap();

        assert_eq!(event.as_log().get("foo"), Some(&Value::from("foo")));
        assert_eq!(event.as_log().get("bar"), Some(&Value::from("is a string")));
        assert_eq!(event.as_log().get("baz"), Some(&Value::from(12)));
    }

    #[test]
    fn check_remap_abort() {
        let event = {
            let mut event = Event::Log(LogEvent::from("augment me"));
            event.as_mut_log().insert("bar", "is a string");
            event
        };

        let conf = RemapConfig {
            source: Some(formatdoc! {r#"
                .foo = "foo"
                abort
                .baz = 12
            "#}),
            file: None,
            drop_on_error: false,
            drop_on_abort: false,
            ..Default::default()
        };
        let mut tform = remap(conf).unwrap();

        let event = transform_one(&mut tform, event).unwrap();

        assert_eq!(event.as_log().get("bar"), Some(&Value::from("is a string")));
        assert!(event.as_log().get("foo").is_none());
        assert!(event.as_log().get("baz").is_none());
    }

    #[test]
    fn check_remap_abort_drop() {
        let event = {
            let mut event = Event::Log(LogEvent::from("augment me"));
            event.as_mut_log().insert("bar", "is a string");
            event
        };

        let conf = RemapConfig {
            source: Some(formatdoc! {r#"
                .foo = "foo"
                abort
                .baz = 12
            "#}),
            file: None,
            drop_on_error: false,
            drop_on_abort: true,
            ..Default::default()
        };
        let mut tform = remap(conf).unwrap();

        assert!(transform_one(&mut tform, event).is_none())
    }

    #[test]
    fn check_remap_metric() {
        let metric = Event::Metric(Metric::new(
            "counter",
            MetricKind::Absolute,
            MetricValue::Counter { value: 1.0 },
        ));
        let metadata = metric.metadata().clone();

        let conf = RemapConfig {
            source: Some(
                r#".tags.host = "zoobub"
                       .name = "zork"
                       .namespace = "zerk"
                       .kind = "incremental""#
                    .to_string(),
            ),
            file: None,
            drop_on_error: true,
            drop_on_abort: false,
            ..Default::default()
        };
        let mut tform = remap(conf).unwrap();

        let result = transform_one(&mut tform, metric).unwrap();
        assert_eq!(
            result,
            Event::Metric(
                Metric::new_with_metadata(
                    "zork",
                    MetricKind::Incremental,
                    MetricValue::Counter { value: 1.0 },
                    metadata.with_schema_definition(&Arc::new(test_default_schema_definition())),
                )
                .with_namespace(Some("zerk"))
                .with_tags(Some(metric_tags! {
                    "host" => "zoobub",
                }))
            )
        );
    }

    #[test]
    fn remap_timezone_fallback() {
        let error =
            Event::try_from(serde_json::json!({"timestamp": "2022-12-27 00:00:00"})).unwrap();
        let conf = RemapConfig {
            source: Some(formatdoc! {r#"
                .timestamp = parse_timestamp!(.timestamp, format: "%Y-%m-%d %H:%M:%S")
            "#}),
            drop_on_error: true,
            drop_on_abort: true,
            reroute_dropped: true,
            ..Default::default()
        };
        let context = TransformContext {
            key: Some(ComponentKey::from("remapper")),
            globals: GlobalOptions {
                timezone: Some(TimeZone::parse("America/Los_Angeles").unwrap()),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut tform = Remap::new_ast(conf, &context).unwrap().0;

        let output = transform_one_fallible(&mut tform, error).unwrap();
        let log = output.as_log();
        assert_eq!(
            log["timestamp"],
            DateTime::<chrono::Utc>::from(
                DateTime::parse_from_rfc3339("2022-12-27T00:00:00-08:00").unwrap()
            )
            .into()
        );
    }

    #[test]
    fn remap_timezone_override() {
        let error =
            Event::try_from(serde_json::json!({"timestamp": "2022-12-27 00:00:00"})).unwrap();
        let conf = RemapConfig {
            source: Some(formatdoc! {r#"
                .timestamp = parse_timestamp!(.timestamp, format: "%Y-%m-%d %H:%M:%S")
            "#}),
            drop_on_error: true,
            drop_on_abort: true,
            reroute_dropped: true,
            timezone: Some(TimeZone::parse("America/Los_Angeles").unwrap()),
            ..Default::default()
        };
        let context = TransformContext {
            key: Some(ComponentKey::from("remapper")),
            globals: GlobalOptions {
                timezone: Some(TimeZone::parse("Etc/UTC").unwrap()),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut tform = Remap::new_ast(conf, &context).unwrap().0;

        let output = transform_one_fallible(&mut tform, error).unwrap();
        let log = output.as_log();
        assert_eq!(
            log["timestamp"],
            DateTime::<chrono::Utc>::from(
                DateTime::parse_from_rfc3339("2022-12-27T00:00:00-08:00").unwrap()
            )
            .into()
        );
    }

    #[test]
    fn check_remap_branching() {
        let happy = Event::try_from(serde_json::json!({"hello": "world"})).unwrap();
        let abort = Event::try_from(serde_json::json!({"hello": "goodbye"})).unwrap();
        let error = Event::try_from(serde_json::json!({"hello": 42})).unwrap();

        let happy_metric = {
            let mut metric = Metric::new(
                "counter",
                MetricKind::Absolute,
                MetricValue::Counter { value: 1.0 },
            );
            metric.replace_tag("hello".into(), "world".into());
            Event::Metric(metric)
        };

        let abort_metric = {
            let mut metric = Metric::new(
                "counter",
                MetricKind::Absolute,
                MetricValue::Counter { value: 1.0 },
            );
            metric.replace_tag("hello".into(), "goodbye".into());
            Event::Metric(metric)
        };

        let error_metric = {
            let mut metric = Metric::new(
                "counter",
                MetricKind::Absolute,
                MetricValue::Counter { value: 1.0 },
            );
            metric.replace_tag("not_hello".into(), "oops".into());
            Event::Metric(metric)
        };

        let conf = RemapConfig {
            source: Some(formatdoc! {r#"
                if exists(.tags) {{
                    # metrics
                    .tags.foo = "bar"
                    if string!(.tags.hello) == "goodbye" {{
                      abort
                    }}
                }} else {{
                    # logs
                    .foo = "bar"
                    if string(.hello) == "goodbye" {{
                      abort
                    }}
                }}
            "#}),
            drop_on_error: true,
            drop_on_abort: true,
            reroute_dropped: true,
            ..Default::default()
        };
        let schema_definitions = HashMap::from([
            (
                None,
                [("source".into(), test_default_schema_definition())].into(),
            ),
            (
                Some(DROPPED.to_owned()),
                [("source".into(), test_dropped_schema_definition())].into(),
            ),
        ]);
        let context = TransformContext {
            key: Some(ComponentKey::from("remapper")),
            schema_definitions,
            merged_schema_definition: schema::Definition::new_with_default_metadata(
                Kind::any_object(),
                [LogNamespace::Legacy],
            )
            .with_event_field(&owned_value_path!("hello"), Kind::bytes(), None),
            ..Default::default()
        };
        let mut tform = Remap::new_ast(conf, &context).unwrap().0;

        let output = transform_one_fallible(&mut tform, happy).unwrap();
        let log = output.as_log();
        assert_eq!(log["hello"], "world".into());
        assert_eq!(log["foo"], "bar".into());
        assert!(!log.contains("metadata"));

        let output = transform_one_fallible(&mut tform, abort).unwrap_err();
        let log = output.as_log();
        assert_eq!(log["hello"], "goodbye".into());
        assert!(!log.contains("foo"));
        assert_eq!(
            log["metadata"],
            serde_json::json!({
                "dropped": {
                    "reason": "abort",
                    "message": "aborted",
                    "component_id": "remapper",
                    "component_type": "remap",
                    "component_kind": "transform",
                }
            })
            .try_into()
            .unwrap()
        );

        let output = transform_one_fallible(&mut tform, error).unwrap_err();
        let log = output.as_log();
        assert_eq!(log["hello"], 42.into());
        assert!(!log.contains("foo"));
        assert_eq!(
            log["metadata"],
            serde_json::json!({
                "dropped": {
                    "reason": "error",
                    "message": "function call error for \"string\" at (160:174): expected string, got integer",
                    "component_id": "remapper",
                    "component_type": "remap",
                    "component_kind": "transform",
                }
            })
            .try_into()
            .unwrap()
        );

        let output = transform_one_fallible(&mut tform, happy_metric).unwrap();
        similar_asserts::assert_eq!(
            output,
            Event::Metric(
                Metric::new_with_metadata(
                    "counter",
                    MetricKind::Absolute,
                    MetricValue::Counter { value: 1.0 },
                    EventMetadata::default()
                        .with_schema_definition(&Arc::new(test_default_schema_definition())),
                )
                .with_tags(Some(metric_tags! {
                    "hello" => "world",
                    "foo" => "bar",
                }))
            )
        );

        let output = transform_one_fallible(&mut tform, abort_metric).unwrap_err();
        similar_asserts::assert_eq!(
            output,
            Event::Metric(
                Metric::new_with_metadata(
                    "counter",
                    MetricKind::Absolute,
                    MetricValue::Counter { value: 1.0 },
                    EventMetadata::default()
                        .with_schema_definition(&Arc::new(test_dropped_schema_definition())),
                )
                .with_tags(Some(metric_tags! {
                    "hello" => "goodbye",
                    "metadata.dropped.reason" => "abort",
                    "metadata.dropped.component_id" => "remapper",
                    "metadata.dropped.component_type" => "remap",
                    "metadata.dropped.component_kind" => "transform",
                }))
            )
        );

        let output = transform_one_fallible(&mut tform, error_metric).unwrap_err();
        similar_asserts::assert_eq!(
            output,
            Event::Metric(
                Metric::new_with_metadata(
                    "counter",
                    MetricKind::Absolute,
                    MetricValue::Counter { value: 1.0 },
                    EventMetadata::default()
                        .with_schema_definition(&Arc::new(test_dropped_schema_definition())),
                )
                .with_tags(Some(metric_tags! {
                    "not_hello" => "oops",
                    "metadata.dropped.reason" => "error",
                    "metadata.dropped.component_id" => "remapper",
                    "metadata.dropped.component_type" => "remap",
                    "metadata.dropped.component_kind" => "transform",
                }))
            )
        );
    }

    #[test]
    fn check_remap_branching_assert_with_message() {
        let error_trigger_assert_custom_message =
            Event::try_from(serde_json::json!({"hello": 42})).unwrap();
        let error_trigger_default_assert_message =
            Event::try_from(serde_json::json!({"hello": 0})).unwrap();
        let conf = RemapConfig {
            source: Some(formatdoc! {r#"
                assert_eq!(.hello, 0, "custom message here")
                assert_eq!(.hello, 1)
            "#}),
            drop_on_error: true,
            drop_on_abort: true,
            reroute_dropped: true,
            ..Default::default()
        };
        let context = TransformContext {
            key: Some(ComponentKey::from("remapper")),
            ..Default::default()
        };
        let mut tform = Remap::new_ast(conf, &context).unwrap().0;

        let output =
            transform_one_fallible(&mut tform, error_trigger_assert_custom_message).unwrap_err();
        let log = output.as_log();
        assert_eq!(log["hello"], 42.into());
        assert!(!log.contains("foo"));
        assert_eq!(
            log["metadata"],
            serde_json::json!({
                "dropped": {
                    "reason": "error",
                    "message": "custom message here",
                    "component_id": "remapper",
                    "component_type": "remap",
                    "component_kind": "transform",
                }
            })
            .try_into()
            .unwrap()
        );

        let output =
            transform_one_fallible(&mut tform, error_trigger_default_assert_message).unwrap_err();
        let log = output.as_log();
        assert_eq!(log["hello"], 0.into());
        assert!(!log.contains("foo"));
        assert_eq!(
            log["metadata"],
            serde_json::json!({
                "dropped": {
                    "reason": "error",
                    "message": "function call error for \"assert_eq\" at (45:66): assertion failed: 0 == 1",
                    "component_id": "remapper",
                    "component_type": "remap",
                    "component_kind": "transform",
                }
            })
            .try_into()
            .unwrap()
        );
    }

    #[test]
    fn check_remap_branching_abort_with_message() {
        let error = Event::try_from(serde_json::json!({"hello": 42})).unwrap();
        let conf = RemapConfig {
            source: Some(formatdoc! {r#"
                abort "custom message here"
            "#}),
            drop_on_error: true,
            drop_on_abort: true,
            reroute_dropped: true,
            ..Default::default()
        };
        let context = TransformContext {
            key: Some(ComponentKey::from("remapper")),
            ..Default::default()
        };
        let mut tform = Remap::new_ast(conf, &context).unwrap().0;

        let output = transform_one_fallible(&mut tform, error).unwrap_err();
        let log = output.as_log();
        assert_eq!(log["hello"], 42.into());
        assert!(!log.contains("foo"));
        assert_eq!(
            log["metadata"],
            serde_json::json!({
                "dropped": {
                    "reason": "abort",
                    "message": "custom message here",
                    "component_id": "remapper",
                    "component_type": "remap",
                    "component_kind": "transform",
                }
            })
            .try_into()
            .unwrap()
        );
    }

    #[test]
    fn check_remap_branching_disabled() {
        let happy = Event::try_from(serde_json::json!({"hello": "world"})).unwrap();
        let abort = Event::try_from(serde_json::json!({"hello": "goodbye"})).unwrap();
        let error = Event::try_from(serde_json::json!({"hello": 42})).unwrap();

        let conf = RemapConfig {
            source: Some(formatdoc! {r#"
                if exists(.tags) {{
                    # metrics
                    .tags.foo = "bar"
                    if string!(.tags.hello) == "goodbye" {{
                      abort
                    }}
                }} else {{
                    # logs
                    .foo = "bar"
                    if string!(.hello) == "goodbye" {{
                      abort
                    }}
                }}
            "#}),
            drop_on_error: true,
            drop_on_abort: true,
            reroute_dropped: false,
            ..Default::default()
        };

        let schema_definition = schema::Definition::new_with_default_metadata(
            Kind::any_object(),
            [LogNamespace::Legacy],
        )
        .with_event_field(&owned_value_path!("foo"), Kind::any(), None)
        .with_event_field(&owned_value_path!("tags"), Kind::any(), None);

        assert_eq!(
            conf.outputs(
                enrichment::TableRegistry::default(),
                &[(
                    "test".into(),
                    schema::Definition::new_with_default_metadata(
                        Kind::any_object(),
                        [LogNamespace::Legacy]
                    )
                )],
                LogNamespace::Legacy
            ),
            vec![TransformOutput::new(
                DataType::all(),
                [("test".into(), schema_definition)].into()
            )]
        );

        let context = TransformContext {
            key: Some(ComponentKey::from("remapper")),
            ..Default::default()
        };
        let mut tform = Remap::new_ast(conf, &context).unwrap().0;

        let output = transform_one_fallible(&mut tform, happy).unwrap();
        let log = output.as_log();
        assert_eq!(log["hello"], "world".into());
        assert_eq!(log["foo"], "bar".into());
        assert!(!log.contains("metadata"));

        let out = collect_outputs(&mut tform, abort);
        assert!(out.primary.is_empty());
        assert!(out.named[DROPPED].is_empty());

        let out = collect_outputs(&mut tform, error);
        assert!(out.primary.is_empty());
        assert!(out.named[DROPPED].is_empty());
    }

    #[tokio::test]
    async fn check_remap_branching_metrics_with_output() {
        init_test();

        let config: ConfigBuilder = toml::from_str(indoc! {r#"
            [transforms.foo]
            inputs = []
            type = "remap"
            drop_on_abort = true
            reroute_dropped = true
            source = "abort"

            [[tests]]
            name = "metric output"

            [tests.input]
                insert_at = "foo"
                value = "none"

            [[tests.outputs]]
                extract_from = "foo.dropped"
                [[tests.outputs.conditions]]
                type = "vrl"
                source = "true"
        "#})
        .unwrap();

        let mut tests = build_unit_tests(config).await.unwrap();
        assert!(tests.remove(0).run().await.errors.is_empty());
        // Check that metrics were emitted with output tag
        COMPONENT_MULTIPLE_OUTPUTS_TESTS.assert(&["output"]);
    }

    struct CollectedOuput {
        primary: OutputBuffer,
        named: HashMap<String, OutputBuffer>,
    }

    fn collect_outputs(ft: &mut dyn SyncTransform, event: Event) -> CollectedOuput {
        let mut outputs = TransformOutputsBuf::new_with_capacity(
            vec![
                TransformOutput::new(DataType::all(), HashMap::new()),
                TransformOutput::new(DataType::all(), HashMap::new()).with_port(DROPPED),
            ],
            1,
        );

        ft.transform(event, &mut outputs);

        CollectedOuput {
            primary: outputs.take_primary(),
            named: outputs.take_all_named(),
        }
    }

    fn transform_one(ft: &mut dyn SyncTransform, event: Event) -> Option<Event> {
        let out = collect_outputs(ft, event);
        assert_eq!(0, out.named.values().map(|v| v.len()).sum::<usize>());
        assert!(out.primary.len() <= 1);
        out.primary.into_events().next()
    }

    fn transform_one_fallible(
        ft: &mut dyn SyncTransform,
        event: Event,
    ) -> std::result::Result<Event, Event> {
        let mut outputs = TransformOutputsBuf::new_with_capacity(
            vec![
                TransformOutput::new(DataType::all(), HashMap::new()),
                TransformOutput::new(DataType::all(), HashMap::new()).with_port(DROPPED),
            ],
            1,
        );

        ft.transform(event, &mut outputs);

        let mut buf = outputs.drain().collect::<Vec<_>>();
        let mut err_buf = outputs.drain_named(DROPPED).collect::<Vec<_>>();

        assert!(buf.len() < 2);
        assert!(err_buf.len() < 2);
        match (buf.pop(), err_buf.pop()) {
            (Some(good), None) => Ok(good),
            (None, Some(bad)) => Err(bad),
            (a, b) => panic!("expected output xor error output, got {:?} and {:?}", a, b),
        }
    }

    #[tokio::test]
    async fn emits_internal_events() {
        assert_transform_compliance(async move {
            let config = RemapConfig {
                source: Some("abort".to_owned()),
                drop_on_abort: true,
                ..Default::default()
            };

            let (tx, rx) = mpsc::channel(1);
            let (topology, mut out) = create_topology(ReceiverStream::new(rx), config).await;

            let log = LogEvent::from("hello world");
            tx.send(log.into()).await.unwrap();

            drop(tx);
            topology.stop().await;
            assert_eq!(out.recv().await, None);
        })
        .await
    }

    #[test]
    fn test_field_definitions_in_message() {
        let definition =
            schema::Definition::new_with_default_metadata(Kind::bytes(), [LogNamespace::Legacy]);
        assert_eq!(
            schema::Definition::new_with_default_metadata(
                Kind::object(BTreeMap::from([("message".into(), Kind::bytes())])),
                [LogNamespace::Legacy]
            ),
            move_field_definitions_into_message(definition)
        );

        // Test when a message field already exists.
        let definition = schema::Definition::new_with_default_metadata(
            Kind::object(BTreeMap::from([("message".into(), Kind::integer())])).or_bytes(),
            [LogNamespace::Legacy],
        );
        assert_eq!(
            schema::Definition::new_with_default_metadata(
                Kind::object(BTreeMap::from([(
                    "message".into(),
                    Kind::bytes().or_integer()
                )])),
                [LogNamespace::Legacy]
            ),
            move_field_definitions_into_message(definition)
        )
    }

    #[test]
    fn test_merged_array_definitions_simple() {
        // Test merging the array definitions where the schema definition
        // is simple, containing only one possible type in the array.
        let object: BTreeMap<value::kind::Field, Kind> = [
            ("carrot".into(), Kind::bytes()),
            ("potato".into(), Kind::integer()),
        ]
        .into();

        let kind = Kind::array(Collection::from_unknown(Kind::object(object)));

        let definition =
            schema::Definition::new_with_default_metadata(kind, [LogNamespace::Legacy]);

        let kind = Kind::object(BTreeMap::from([
            ("carrot".into(), Kind::bytes()),
            ("potato".into(), Kind::integer()),
        ]));

        let wanted = schema::Definition::new_with_default_metadata(kind, [LogNamespace::Legacy]);
        let merged = merge_array_definitions(definition);

        assert_eq!(wanted, merged);
    }

    #[test]
    fn test_merged_array_definitions_complex() {
        // Test merging the array definitions where the schema definition
        // is fairly complex containing multiple different possible types.
        let object: BTreeMap<value::kind::Field, Kind> = [
            ("carrot".into(), Kind::bytes()),
            ("potato".into(), Kind::integer()),
        ]
        .into();

        let array: BTreeMap<Index, Kind> = [
            (Index::from(0), Kind::integer()),
            (Index::from(1), Kind::boolean()),
            (
                Index::from(2),
                Kind::object(BTreeMap::from([("peas".into(), Kind::bytes())])),
            ),
        ]
        .into();

        let mut kind = Kind::bytes();
        kind.add_object(object);
        kind.add_array(array);

        let definition =
            schema::Definition::new_with_default_metadata(kind, [LogNamespace::Legacy]);

        let mut kind = Kind::bytes();
        kind.add_integer();
        kind.add_boolean();
        kind.add_object(BTreeMap::from([
            ("carrot".into(), Kind::bytes().or_undefined()),
            ("potato".into(), Kind::integer().or_undefined()),
            ("peas".into(), Kind::bytes().or_undefined()),
        ]));

        let wanted = schema::Definition::new_with_default_metadata(kind, [LogNamespace::Legacy]);
        let merged = merge_array_definitions(definition);

        assert_eq!(wanted, merged);
    }

    #[test]
    fn test_combined_transforms_simple() {
        // Make sure that when getting the definitions from one transform and
        // passing them to another the correct definition is still produced.

        // Transform 1 sets a simple value.
        let transform1 = RemapConfig {
            source: Some(r#".thing = "potato""#.to_string()),
            ..Default::default()
        };

        let transform2 = RemapConfig {
            source: Some(".thang = .thing".to_string()),
            ..Default::default()
        };

        let enrichment_tables = enrichment::TableRegistry::default();

        let outputs1 = transform1.outputs(
            enrichment_tables.clone(),
            &[("in".into(), schema::Definition::default_legacy_namespace())],
            LogNamespace::Legacy,
        );

        assert_eq!(
            vec![TransformOutput::new(
                DataType::all(),
                // The `never` definition should have been passed on to the end.
                [(
                    "in".into(),
                    Definition::default_legacy_namespace().with_event_field(
                        &owned_value_path!("thing"),
                        Kind::bytes(),
                        None
                    ),
                )]
                .into()
            )],
            outputs1
        );

        let outputs2 = transform2.outputs(
            enrichment_tables,
            &[(
                "in1".into(),
                outputs1[0].schema_definitions(true)[&"in".into()].clone(),
            )],
            LogNamespace::Legacy,
        );

        assert_eq!(
            vec![TransformOutput::new(
                DataType::all(),
                [(
                    "in1".into(),
                    Definition::default_legacy_namespace()
                        .with_event_field(&owned_value_path!("thing"), Kind::bytes(), None)
                        .with_event_field(&owned_value_path!("thang"), Kind::bytes(), None),
                )]
                .into(),
            )],
            outputs2
        );
    }

    #[test]
    fn test_combined_transforms_unnest() {
        // Make sure that when getting the definitions from one transform and
        // passing them to another the correct definition is still produced.

        // Transform 1 sets a simple value.
        let transform1 = RemapConfig {
            source: Some(
                indoc! {
                r#"
                .thing = [{"cabbage": 32}, {"parsnips": 45}]
                . = unnest(.thing)
                "#
                }
                .to_string(),
            ),
            ..Default::default()
        };

        let transform2 = RemapConfig {
            source: Some(r#".thang = .thing.cabbage || "beetroot""#.to_string()),
            ..Default::default()
        };

        let enrichment_tables = enrichment::TableRegistry::default();

        let outputs1 = transform1.outputs(
            enrichment_tables.clone(),
            &[(
                "in".into(),
                schema::Definition::new_with_default_metadata(
                    Kind::any_object(),
                    [LogNamespace::Legacy],
                ),
            )],
            LogNamespace::Legacy,
        );

        assert_eq!(
            vec![TransformOutput::new(
                DataType::all(),
                [(
                    "in".into(),
                    Definition::new_with_default_metadata(
                        Kind::any_object(),
                        [LogNamespace::Legacy]
                    )
                    .with_event_field(
                        &owned_value_path!("thing"),
                        Kind::object(Collection::from(BTreeMap::from([
                            ("cabbage".into(), Kind::integer().or_undefined(),),
                            ("parsnips".into(), Kind::integer().or_undefined(),)
                        ]))),
                        None
                    ),
                )]
                .into(),
            )],
            outputs1
        );

        let outputs2 = transform2.outputs(
            enrichment_tables,
            &[(
                "in1".into(),
                outputs1[0].schema_definitions(true)[&"in".into()].clone(),
            )],
            LogNamespace::Legacy,
        );

        assert_eq!(
            vec![TransformOutput::new(
                DataType::all(),
                [(
                    "in1".into(),
                    Definition::default_legacy_namespace()
                        .with_event_field(
                            &owned_value_path!("thing"),
                            Kind::object(Collection::from(BTreeMap::from([
                                ("cabbage".into(), Kind::integer().or_undefined(),),
                                ("parsnips".into(), Kind::integer().or_undefined(),)
                            ]))),
                            None
                        )
                        .with_event_field(
                            &owned_value_path!("thang"),
                            Kind::integer().or_null(),
                            None
                        ),
                )]
                .into(),
            )],
            outputs2
        );
    }

    #[test]
    fn test_transform_abort() {
        // An abort should not change the typedef.

        let transform1 = RemapConfig {
            source: Some(r#"abort"#.to_string()),
            ..Default::default()
        };

        let enrichment_tables = enrichment::TableRegistry::default();

        let outputs1 = transform1.outputs(
            enrichment_tables,
            &[(
                "in".into(),
                schema::Definition::new_with_default_metadata(
                    Kind::any_object(),
                    [LogNamespace::Legacy],
                ),
            )],
            LogNamespace::Legacy,
        );

        assert_eq!(
            vec![TransformOutput::new(
                DataType::all(),
                [(
                    "in".into(),
                    Definition::new_with_default_metadata(
                        Kind::any_object(),
                        [LogNamespace::Legacy]
                    ),
                )]
                .into(),
            )],
            outputs1
        );
    }

    #[test]
    fn test_error_outputs() {
        // Even if we fail to compile the VRL it should still output
        // the correct ports. This may change if we separate the
        // `outputs` function into one returning outputs and a separate
        // returning schema definitions.
        let transform1 = RemapConfig {
            // This enrichment table does not exist.
            source: Some(r#". |= get_enrichment_table_record("carrot", {"id": .id})"#.to_string()),
            reroute_dropped: true,
            ..Default::default()
        };

        let enrichment_tables = enrichment::TableRegistry::default();

        let outputs1 = transform1.outputs(
            enrichment_tables,
            &[(
                "in".into(),
                schema::Definition::new_with_default_metadata(
                    Kind::any_object(),
                    [LogNamespace::Legacy],
                ),
            )],
            LogNamespace::Legacy,
        );

        assert_eq!(
            HashSet::from([None, Some("dropped".to_string())]),
            outputs1
                .into_iter()
                .map(|output| output.port)
                .collect::<HashSet<_>>()
        );
    }

    #[test]
    fn test_non_object_events() {
        let transform1 = RemapConfig {
            // This enrichment table does not exist.
            source: Some(r#". = "fish" "#.to_string()),
            ..Default::default()
        };

        let enrichment_tables = enrichment::TableRegistry::default();

        let outputs1 = transform1.outputs(
            enrichment_tables,
            &[(
                "in".into(),
                schema::Definition::new_with_default_metadata(
                    Kind::any_object(),
                    [LogNamespace::Legacy],
                ),
            )],
            LogNamespace::Legacy,
        );

        let wanted = schema::Definition::new_with_default_metadata(
            Kind::object(Collection::from_unknown(Kind::undefined())),
            [LogNamespace::Legacy],
        )
        .with_event_field(&owned_value_path!("message"), Kind::bytes(), None);

        assert_eq!(
            HashMap::from([(OutputId::from("in"), wanted)]),
            outputs1[0].schema_definitions(true),
        );
    }

    #[test]
    fn test_array_and_non_object_events() {
        let transform1 = RemapConfig {
            source: Some(
                indoc! {r#"
                    if .lizard == true {
                        .thing = [{"cabbage": 42}];
                        . = unnest(.thing)
                    } else {
                      . = "fish"
                    }
                    "#}
                .to_string(),
            ),
            ..Default::default()
        };

        let enrichment_tables = enrichment::TableRegistry::default();

        let outputs1 = transform1.outputs(
            enrichment_tables,
            &[(
                "in".into(),
                schema::Definition::new_with_default_metadata(
                    Kind::any_object(),
                    [LogNamespace::Legacy],
                ),
            )],
            LogNamespace::Legacy,
        );

        let wanted = schema::Definition::new_with_default_metadata(
            Kind::any_object(),
            [LogNamespace::Legacy],
        )
        .with_event_field(&owned_value_path!("message"), Kind::any(), None)
        .with_event_field(
            &owned_value_path!("thing"),
            Kind::object(Collection::from(BTreeMap::from([(
                "cabbage".into(),
                Kind::integer(),
            )])))
            .or_undefined(),
            None,
        );

        assert_eq!(
            HashMap::from([(OutputId::from("in"), wanted)]),
            outputs1[0].schema_definitions(true),
        );
    }
}
