import 'common.dart';
import 'classic/relational/relational_boundary.dart';
import 'classic/relational/comparison_negation.dart';
import 'classic/relational/comparison_operator_replacement.dart';
import 'classic/logical/logical_connector_swap.dart';
import 'classic/logical/logical_operand_removal.dart';
import 'classic/arithmetic/arithmetic_operator_replacement.dart';
import 'classic/arithmetic/arithmetic_operand_removal.dart';
import 'classic/arithmetic/numeric_negation.dart';
import 'classic/arithmetic/increment_decrement_replacement.dart';
import 'classic/arithmetic/compound_assignment_replacement.dart';
import 'classic/boolean/boolean_literal_flip.dart';
import 'classic/boolean/boolean_negation.dart';
import 'classic/boolean/conditional_negation.dart';
import 'classic/constants/numeric_literal_replacement.dart';
import 'classic/constants/string_literal_replacement.dart';
import 'classic/returns/return_value_replacement.dart';
import 'classic/control_flow/condition_forcing.dart';
import 'classic/control_flow/branch_body_removal.dart';
import 'classic/control_flow/conditional_expression_mutation.dart';
import 'classic/control_flow/statement_deletion.dart';
import 'classic/control_flow/break_continue_mutation.dart';
import 'classic/control_flow/switch_case_mutation.dart';
import 'classic/calls/function_call_removal.dart';
import 'classic/calls/argument_replacement.dart';
import 'classic/calls/argument_permutation.dart';
import 'classic/variables/variable_replacement.dart';
import 'classic/bitwise/bitwise_operator_replacement.dart';
import 'classic/bitwise/bitwise_negation.dart';
import 'classic/bitwise/shift_operator_replacement.dart';
import 'classic/exceptions/throw_removal.dart';
import 'classic/exceptions/exception_replacement.dart';
import 'classic/collections/collection_literal_mutation.dart';
import 'failure_modes/exception_at_entry.dart';
import 'failure_modes/method_channel_error.dart';

/// The user-facing hierarchy within the classic family.
const Map<ClassicalCategory, List<ClassicOperator>>
operatorsByClassicalCategory = {
  ClassicalCategory.relational: [
    RelationalBoundary(),
    ComparisonNegation(),
    ComparisonOperatorReplacement(),
  ],
  ClassicalCategory.logical: [LogicalConnectorSwap(), LogicalOperandRemoval()],
  ClassicalCategory.arithmetic: [
    ArithmeticOperatorReplacement(),
    ArithmeticOperandRemoval(),
    NumericNegation(),
    IncrementDecrementReplacement(),
    CompoundAssignmentReplacement(),
  ],
  ClassicalCategory.boolean: [
    BooleanLiteralFlip(),
    BooleanNegation(),
    ConditionalNegation(),
  ],
  ClassicalCategory.constants: [
    NumericLiteralReplacement(),
    StringLiteralReplacement(),
  ],
  ClassicalCategory.returns: [ReturnValueReplacement()],
  ClassicalCategory.controlFlow: [
    ConditionForcing(),
    BranchBodyRemoval(),
    ConditionalExpressionMutation(),
    StatementDeletion(),
    BreakContinueMutation(),
    SwitchCaseMutation(),
  ],
  ClassicalCategory.calls: [
    FunctionCallRemoval(),
    ArgumentReplacement(),
    ArgumentPermutation(),
  ],
  ClassicalCategory.variables: [VariableReplacement()],
  ClassicalCategory.bitwise: [
    BitwiseOperatorReplacement(),
    BitwiseNegation(),
    ShiftOperatorReplacement(),
  ],
  ClassicalCategory.exceptions: [ThrowRemoval(), ExceptionReplacement()],
  ClassicalCategory.collections: [CollectionLiteralMutation()],
};

/// Includes the explicitly supported legacy conditional operator.
final List<MutationOperator> classicOperators = List.unmodifiable(
  operatorsByClassicalCategory.values.expand((operators) => operators),
);

/// The 31 classical catalogue operators; legacy configurations remain accepted.
final List<MutationOperator> defaultClassicOperators = List.unmodifiable(
  classicOperators.where((operator) => !operator.isLegacy),
);

/// Failure modes require an explicit operator and positive scope selection.
const List<MutationOperator> failureModeOperators = [
  ExceptionAtEntry(),
  MethodChannelError(),
];

final Map<MutationFamily, List<MutationOperator>> operatorsByFamily =
    Map.unmodifiable({
      MutationFamily.classic: classicOperators,
      MutationFamily.failureMode: failureModeOperators,
    });
