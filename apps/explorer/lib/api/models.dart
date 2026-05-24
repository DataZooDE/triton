// Hand-rolled models for Triton's HTTP surface. Avoiding
// `freezed`/`json_serializable` keeps the build pipeline simple for
// a small API; if the surface grows past ~10 types, switch.

class ToolDescriptor {
  ToolDescriptor({
    required this.name,
    required this.inputSchema,
    required this.returnsA2ui,
  });

  final String name;
  final Map<String, dynamic> inputSchema;
  final bool returnsA2ui;

  factory ToolDescriptor.fromJson(Map<String, dynamic> json) => ToolDescriptor(
        name: json['name'] as String,
        // Triton returns the schema under `input_schema` for REST and
        // `inputSchema` for MCP; the SPA only talks to REST today so
        // we take the snake_case key.
        inputSchema: (json['input_schema'] as Map?)?.cast<String, dynamic>() ??
            <String, dynamic>{},
        returnsA2ui: (json['returns_a2ui'] as bool?) ?? false,
      );
}

/// Result of a tool invocation as seen by the SPA. The dispatcher
/// returns `{result, trace_id, returns_a2ui, ...}`; we keep the raw
/// JSON around for the playground's raw-response tab.
class InvocationResult {
  InvocationResult({
    required this.raw,
    required this.statusCode,
    required this.elapsed,
    this.traceId,
    this.error,
  });

  final Map<String, dynamic> raw;
  final int statusCode;
  final Duration elapsed;
  final String? traceId;
  final String? error;

  bool get ok => error == null && statusCode >= 200 && statusCode < 300;
}
