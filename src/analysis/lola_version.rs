use super::super::ast::*;
use crate::reporting::Handler;
use crate::reporting::LabeledSpan;
use ast_node::{AstNode, NodeId, Span};
use std::collections::HashMap;

pub(crate) type LolaVersionTable = HashMap<NodeId, LanguageSpec>;
type WhyNot = (Span, String);

struct VersionTracker {
    pub cannot_be_classic: Option<WhyNot>,
    pub cannot_be_lola2: Option<WhyNot>,
}

impl VersionTracker {
    fn new() -> Self {
        VersionTracker {
            cannot_be_classic: None,
            cannot_be_lola2: None,
        }
    }
    fn from_stream(is_not_parameterized: Option<WhyNot>) -> Self {
        VersionTracker {
            cannot_be_classic: is_not_parameterized,
            cannot_be_lola2: None,
        }
    }
}

fn analyse_expression(
    version_tracker: &mut VersionTracker,
    expr: &Expression,
    toplevel_in_trigger: bool,
) {
    match &expr.kind {
        ExpressionKind::Lit(_) => {}
        ExpressionKind::Ident(_) => {}
        ExpressionKind::Default(target, default) => {
            analyse_expression(version_tracker, &*target, false);
            analyse_expression(version_tracker, &*default, false);
        }
        ExpressionKind::Lookup(_, offset, _) => match offset {
            Offset::DiscreteOffset(expr) => {
                analyse_expression(version_tracker, &*expr, false);
            }
            Offset::RealTimeOffset(offset, _) => {
                analyse_expression(version_tracker, &*offset, false);
                version_tracker.cannot_be_lola2 =
                    Some((*expr.span(), String::from("Real time offset – no Lola2")));
                version_tracker.cannot_be_classic = Some((
                    *expr.span(),
                    String::from("Real time offset – no ClassicLola"),
                ));
            }
        },
        ExpressionKind::Binary(_, left, right) => {
            analyse_expression(version_tracker, &*left, false);
            analyse_expression(version_tracker, &*right, false);
        }
        ExpressionKind::Unary(_, nested) => {
            analyse_expression(version_tracker, &*nested, false);
        }
        ExpressionKind::Ite(condition, if_case, else_case) => {
            analyse_expression(version_tracker, &*condition, false);
            analyse_expression(version_tracker, &*if_case, false);
            analyse_expression(version_tracker, &*else_case, false);
        }
        ExpressionKind::ParenthesizedExpression(_, nested, _) => {
            analyse_expression(version_tracker, &*nested, false);
        }
        ExpressionKind::MissingExpression() => {}
        ExpressionKind::Tuple(nested_exprs) => {
            nested_exprs.iter().for_each(|nested| {
                analyse_expression(version_tracker, &*nested, false);
            });
        }
        ExpressionKind::Function(_, _, arguments) => {
            arguments.iter().for_each(|arg| {
                analyse_expression(version_tracker, &*arg, false);
            });
        }
        ExpressionKind::Field(expr, _) => analyse_expression(version_tracker, expr, false),
        ExpressionKind::Method(expr, _, _, args) => {
            analyse_expression(version_tracker, expr, false);
            args.iter().for_each(|arg| {
                analyse_expression(version_tracker, arg, false);
            });
        }
    }
}

pub(crate) struct LolaVersionAnalysis<'a> {
    pub result: LolaVersionTable,
    handler: &'a Handler,
}

impl<'a> LolaVersionAnalysis<'a> {
    pub(crate) fn new(handler: &'a Handler) -> Self {
        LolaVersionAnalysis {
            result: HashMap::new(),
            handler,
        }
    }

    fn analyse_input(&mut self, input: &'a Input) {
        if input.params.is_empty() {
            self.result.insert(*input.id(), LanguageSpec::Classic);
        } else {
            self.result.insert(*input.id(), LanguageSpec::Lola2);
        }
    }

    fn analyse_output(&mut self, output: &'a Output) {
        let is_not_parameterized = if output.params.is_empty() {
            None
        } else {
            Some((output.name.span, String::from("Parameterized stream")))
        };
        let mut version_tracker = VersionTracker::from_stream(is_not_parameterized);
        analyse_expression(&mut version_tracker, &output.expression, false);

        // TODO check parameters for InvocationType
        // TODO check extend for frequency

        if version_tracker.cannot_be_classic.is_none() {
            self.result.insert(*output.id(), LanguageSpec::Classic);
            return;
        }
        if version_tracker.cannot_be_lola2.is_none() {
            self.result.insert(*output.id(), LanguageSpec::Lola2);
            return;
        }
        self.result.insert(*output.id(), LanguageSpec::RTLola);
    }

    fn analyse_trigger(&mut self, trigger: &'a Trigger) {
        let mut version_tracker = VersionTracker::new();
        analyse_expression(&mut version_tracker, &trigger.expression, true);

        if version_tracker.cannot_be_classic.is_none() {
            self.result.insert(*trigger.id(), LanguageSpec::Classic);
            return;
        }
        if version_tracker.cannot_be_lola2.is_none() {
            self.result.insert(*trigger.id(), LanguageSpec::Lola2);
            return;
        }
        self.result.insert(*trigger.id(), LanguageSpec::RTLola);
    }

    pub(crate) fn analyse(&mut self, spec: &'a LolaSpec) -> Option<LanguageSpec> {
        let number_of_previous_errors = self.handler.emitted_errors();
        // analyse each stream/trigger to find out their minimal Lola version
        for input in &spec.inputs {
            self.analyse_input(&input);
        }
        for output in &spec.outputs {
            self.analyse_output(&output);
        }
        for trigger in &spec.trigger {
            self.analyse_trigger(&trigger);
        }

        if number_of_previous_errors != self.handler.emitted_errors() {
            return None;
        }

        // each stream/trigger can be attributed to some (minimal) Lola version but the different versions might be incompatible.
        // Therefore iterate again over all streams and triggers and record reasons against the various versions.
        let mut reason_against_classic_lola: Option<WhyNot> = None;
        let mut reason_against_lola2: Option<WhyNot> = None;

        self.rule_out_versions_based_on_inputs(&spec, &mut reason_against_classic_lola);

        self.rule_out_versions_based_on_outputs(
            &spec,
            &mut reason_against_classic_lola,
            &mut reason_against_lola2,
        );
        self.rule_out_versions_based_on_triggers(
            &spec,
            &mut reason_against_classic_lola,
            &mut reason_against_lola2,
        );

        // Try to use the minimal Lola version or give an error containing the reasons why none of the versions is possible.
        if reason_against_classic_lola.is_none() {
            return Some(LanguageSpec::Classic);
        }
        if reason_against_lola2.is_none() {
            return Some(LanguageSpec::Lola2);
        }
        Some(LanguageSpec::RTLola)
    }

    fn rule_out_versions_based_on_triggers(
        &mut self,
        spec: &LolaSpec,
        reason_against_classic_lola: &mut Option<WhyNot>,
        reason_against_lola2: &mut Option<WhyNot>,
    ) {
        for trigger in &spec.trigger {
            let span = match trigger.name {
                None => *trigger.span(),
                Some(ref trigger_name) => trigger_name.span,
            };
            match &self.result[trigger.id()] {
                LanguageSpec::Classic => {}
                LanguageSpec::Lola2 => {
                    if reason_against_classic_lola.is_none() {
                        *reason_against_classic_lola = Some((
                            span,
                            "Classic Lola is not possible due to this being a Lola2 trigger."
                                .to_string(),
                        ))
                    }
                }
                LanguageSpec::RTLola => {
                    if reason_against_classic_lola.is_none() {
                        *reason_against_classic_lola = Some((
                            span,
                            "Classic Lola is not possible due to this being a RTLola trigger."
                                .to_string(),
                        ))
                    }
                    if reason_against_lola2.is_none() {
                        *reason_against_lola2 = Some((
                            span,
                            "Lola2 is not possible due to this being a RTLola trigger.".to_string(),
                        ))
                    }
                }
            }
        }
    }

    fn rule_out_versions_based_on_outputs(
        &mut self,
        spec: &LolaSpec,
        reason_against_classic_lola: &mut Option<WhyNot>,
        reason_against_lola2: &mut Option<WhyNot>,
    ) {
        for output in &spec.outputs {
            let span = output.name.span;
            match &self.result[output.id()] {
                LanguageSpec::Classic => {}
                LanguageSpec::Lola2 => {
                    if reason_against_classic_lola.is_none() {
                        *reason_against_classic_lola = Some((
                            span,
                            format!(
                                "Classic Lola is not possible due to {} being a Lola2 stream.",
                                output.name.name
                            ),
                        ))
                    }
                }
                LanguageSpec::RTLola => {
                    if reason_against_classic_lola.is_none() {
                        *reason_against_classic_lola = Some((
                            span,
                            format!(
                                "Classic Lola is not possible due to {} being a RTLola stream.",
                                output.name.name
                            ),
                        ))
                    }
                    if reason_against_lola2.is_none() {
                        *reason_against_lola2 = Some((
                            span,
                            format!(
                                "Lola2 is not possible due to {} being a RTLola stream.",
                                output.name.name
                            ),
                        ))
                    }
                }
            }
        }
    }

    fn rule_out_versions_based_on_inputs(
        &mut self,
        spec: &LolaSpec,
        reason_against_classic_lola: &mut Option<WhyNot>,
    ) {
        for input in &spec.inputs {
            match &self.result[input.id()] {
                LanguageSpec::Classic => {}
                LanguageSpec::Lola2 => {
                    if reason_against_classic_lola.is_none() {
                        *reason_against_classic_lola =
                            Some((input.name.span, String::from("Parameterized input stream")));
                    }
                }
                _ => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::id_assignment;
    use crate::parse::parse;
    use crate::parse::SourceMapper;
    use crate::util::get_node_id_from_spec;
    use std::path::PathBuf;

    #[derive(Debug, Clone, Copy)]
    enum StreamIndex {
        Out(usize),
        In(usize),
        Trig(usize),
    }

    /// Parses the content, runs naming analysis, and check expected number of errors and version
    fn check_version(
        content: &str,
        expected_errors: usize,
        expected_version: Option<LanguageSpec>,
        expected_versions: Vec<(StreamIndex, LanguageSpec)>,
    ) {
        let mut ast = parse(content).unwrap_or_else(|e| panic!("{}", e));
        id_assignment::assign_ids(&mut ast);
        let handler = Handler::new(SourceMapper::new(PathBuf::new(), content));
        let mut version_analyzer = LolaVersionAnalysis::new(&handler);
        let version = version_analyzer.analyse(&ast);
        assert_eq!(expected_errors, handler.emitted_errors());
        assert_eq!(expected_version, version);
        for (index, version) in expected_versions {
            let node_id = match index {
                StreamIndex::In(i) => ast.inputs[i]._id,
                StreamIndex::Out(i) => ast.outputs[i]._id,
                StreamIndex::Trig(i) => ast.trigger[i]._id,
            };
            let actual_version = version_analyzer
                .result
                .get(&node_id)
                .unwrap_or_else(|| panic!("There is no version for this NodeId in the result",));
            assert_eq!(
                version, *actual_version,
                "The expected version and the actual version do not match."
            );
        }
    }

    // TODO: implement test cases
    #[test]
    fn parameterized_output_stream_causes_lola2() {
        check_version(
            "output test<ab: Int8, c: Int8>: Int8 := 3",
            0,
            Some(LanguageSpec::Lola2),
            vec![(StreamIndex::Out(0), LanguageSpec::Lola2)],
        )
    }

    #[test]
    fn time_offset_causes_rtlola() {
        check_version(
            "output test: Int8 := stream[3s]",
            0,
            Some(LanguageSpec::RTLola),
            vec![(StreamIndex::Out(0), LanguageSpec::RTLola)],
        )
    }

    #[test]
    fn simple_trigger_causes_lola() {
        check_version(
            "trigger test := false",
            0,
            Some(LanguageSpec::Classic),
            vec![(StreamIndex::Trig(0), LanguageSpec::Classic)],
        )
    }


    #[test]
    fn time_offset_in_trigger_causes_rtlola() {
        check_version(
            "trigger test := stream[3s]",
            0,
            Some(LanguageSpec::RTLola),
            vec![(StreamIndex::Trig(0), LanguageSpec::RTLola)],
        )
    }

    #[test]
    fn parameterized_input_stream_causes_lola2() {
        check_version(
            "input test<ab: Int8, c: Int8> : Int8",
            0,
            Some(LanguageSpec::Lola2),
            vec![(StreamIndex::In(0), LanguageSpec::Lola2)],
        )
    }
}
