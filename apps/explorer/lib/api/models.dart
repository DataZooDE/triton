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
    this.errorCode,
    this.taskState,
  });

  final Map<String, dynamic> raw;
  final int statusCode;
  final Duration elapsed;
  final String? traceId;
  final String? error;

  /// MCP JSON-RPC error code (e.g. -32602 validation, -32001 auth).
  /// MCP signals tool/auth/validation failures inside an HTTP-200
  /// envelope, so `statusCode` alone can't convey them — the
  /// error-taxonomy view reads this. REST/A2A leave it null (their
  /// taxonomy rides the HTTP status).
  final int? errorCode;

  /// A2A only. The A2A adapter tracks `trace_id → {completed|failed}`
  /// in its in-process task store (FR-A-7) and echoes the terminal
  /// state in `metadata.task_state` on success. REST/MCP leave this
  /// null — the field exists so the Adapters page can show that A2A
  /// carries task lifecycle the other two protocols don't.
  final String? taskState;

  bool get ok => error == null && statusCode >= 200 && statusCode < 300;
}

/// MCP `initialize` handshake result. Triton negotiates a protocol
/// version from its supported set and advertises which capabilities
/// (tools, resources) it serves.
class McpServerInfo {
  McpServerInfo({
    required this.protocolVersion,
    required this.serverName,
    required this.serverVersion,
    required this.capabilities,
  });

  final String protocolVersion;
  final String serverName;
  final String serverVersion;
  final Map<String, dynamic> capabilities;

  factory McpServerInfo.fromResult(Map<String, dynamic> result) {
    final info = (result['serverInfo'] as Map?)?.cast<String, dynamic>() ??
        const <String, dynamic>{};
    return McpServerInfo(
      protocolVersion: (result['protocolVersion'] as String?) ?? '',
      serverName: (info['name'] as String?) ?? '',
      serverVersion: (info['version'] as String?) ?? '',
      capabilities: (result['capabilities'] as Map?)?.cast<String, dynamic>() ??
          const <String, dynamic>{},
    );
  }
}

/// A single content item from MCP `resources/read`. Triton serves a
/// runtime-discovery stub at `ui://triton/runtime.html`.
class McpResource {
  McpResource({
    required this.uri,
    required this.mimeType,
    required this.text,
  });

  final String uri;
  final String mimeType;
  final String text;

  factory McpResource.fromResult(Map<String, dynamic> result) {
    final contents = ((result['contents'] as List?) ?? const []).cast<Map>();
    final first = contents.isEmpty
        ? const <String, dynamic>{}
        : contents.first.cast<String, dynamic>();
    return McpResource(
      uri: (first['uri'] as String?) ?? '',
      mimeType: (first['mimeType'] as String?) ?? '',
      text: (first['text'] as String?) ?? '',
    );
  }
}
