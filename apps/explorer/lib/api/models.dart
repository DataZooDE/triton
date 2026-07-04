// Hand-rolled models for Triton's HTTP surface. Avoiding
// `freezed`/`json_serializable` keeps the build pipeline simple for
// a small API; if the surface grows past ~10 types, switch.

class ToolDescriptor {
  ToolDescriptor({
    required this.name,
    required this.inputSchema,
    required this.returnsA2ui,
    this.upstream = false,
  });

  final String name;
  final Map<String, dynamic> inputSchema;
  final bool returnsA2ui;

  /// True for an upstream agent Triton dispatches into (a Consul
  /// `agent:<name>` service) rather than an in-process tool. Triton
  /// doesn't know its schema, so [inputSchema] is empty.
  final bool upstream;

  factory ToolDescriptor.fromJson(Map<String, dynamic> json) => ToolDescriptor(
        name: json['name'] as String,
        // Triton returns the schema under `input_schema` for REST and
        // `inputSchema` for MCP; the SPA only talks to REST today so
        // we take the snake_case key.
        inputSchema: (json['input_schema'] as Map?)?.cast<String, dynamic>() ??
            <String, dynamic>{},
        returnsA2ui: (json['returns_a2ui'] as bool?) ?? false,
        upstream: (json['upstream'] as bool?) ?? false,
      );
}

/// The exact HTTP frame a protocol client would send for one tool call
/// — surfaced so the Console can show it, let the operator edit it, and
/// send precisely what they typed (message injection / small
/// corrections). `body` is the editable JSON: for REST it is the raw
/// args object; for MCP the full JSON-RPC frame; for A2A the
/// `Message{parts, metadata}`. `headers` carries only the non-secret
/// headers (Accept, Content-Type) — the Authorization bearer is added at
/// send time and never shown.
class WireRequest {
  WireRequest({
    required this.protocol,
    required this.method,
    required this.url,
    required this.headers,
    required this.body,
  });

  /// 'REST' | 'MCP' | 'A2A'.
  final String protocol;
  final String method;
  final String url;
  final Map<String, String> headers;
  final Map<String, dynamic> body;

  WireRequest withBody(Map<String, dynamic> newBody) => WireRequest(
        protocol: protocol,
        method: method,
        url: url,
        headers: headers,
        body: newBody,
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
    this.uiResourceUri,
  });

  final Map<String, dynamic> raw;
  final int statusCode;
  final Duration elapsed;
  final String? traceId;
  final String? error;

  /// An MCP-Apps UI resource the result points at, lifted from the tool
  /// result's `_meta.ui.resourceUri` (e.g. `ui://peacock/<report>`). Set
  /// when the upstream is an interactive renderer (#143); the Console can
  /// fetch it via `resources/read` and embed it in a sandboxed iframe.
  /// Null for ordinary tools.
  final String? uiResourceUri;

  /// Lift `_meta.ui.resourceUri` out of a (possibly wrapped) tool result.
  /// Accepts either the inner tool result or a REST envelope that nests it
  /// under `result`. Falls back to a surface BUTTON that references a
  /// `ui://` resource (the agent's report-as-surface pattern) so the
  /// referenced report renders inline WITHOUT a click — the resource URI
  /// carries the render params since peacock #16, making it
  /// self-sufficient.
  static String? uiFrom(Map<String, dynamic>? result) {
    if (result == null) return null;
    final inner = (result['result'] as Map?)?.cast<String, dynamic>() ?? result;
    for (final level in [inner, result]) {
      final ui = (level['_meta'] as Map?)?['ui'];
      final uri = (ui as Map?)?['resourceUri'];
      if (uri is String && uri.isNotEmpty) return uri;
    }
    return _buttonResource(result, 0);
  }

  /// Bounded scan for the first surface component carrying a `ui://`
  /// `resource` — protocol-agnostic (REST nests the surface under
  /// `result`, Triton's MCP envelope under `structuredContent.result`).
  static String? _buttonResource(Object? node, int depth) {
    if (depth > 8) return null;
    if (node is Map) {
      // Raw surfaces carry `components`; the negotiated A2UI envelope
      // re-shapes them onto `stream` — the resource field rides both.
      for (final key in const ['components', 'stream']) {
        final comps = node[key];
        if (comps is List) {
          for (final c in comps) {
            final r = (c is Map) ? c['resource'] : null;
            if (r is String && r.startsWith('ui://')) return r;
          }
        }
      }
      for (final v in node.values) {
        final r = _buttonResource(v, depth + 1);
        if (r != null) return r;
      }
    } else if (node is List) {
      for (final v in node) {
        final r = _buttonResource(v, depth + 1);
        if (r != null) return r;
      }
    }
    return null;
  }

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
