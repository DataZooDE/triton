import 'package:dio/dio.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:triton_explorer/api/a2a_client.dart';
import 'package:triton_explorer/api/mcp_client.dart';
import 'package:triton_explorer/api/rest_client.dart';

class _RecordingAdapter implements HttpClientAdapter {
  final List<RequestOptions> calls = [];
  ResponseBody Function(RequestOptions) responder = (_) => ResponseBody.fromString(
        '{}',
        200,
        headers: {
          Headers.contentTypeHeader: ['application/json'],
        },
      );

  @override
  Future<ResponseBody> fetch(
      RequestOptions options, Stream<List<int>>? body, Future<void>? cancel) async {
    calls.add(options);
    return responder(options);
  }

  @override
  void close({bool force = false}) {}
}

void main() {
  group('RestClient', () {
    test('listTools maps /v1/tools shape', () async {
      final dio = Dio();
      final adapter = _RecordingAdapter();
      adapter.responder = (req) => ResponseBody.fromString(
            '{"tools":[{"name":"echo","input_schema":{"type":"object"},'
            '"returns_a2ui":false}]}',
            200,
            headers: {
              Headers.contentTypeHeader: ['application/json'],
            },
          );
      dio.httpClientAdapter = adapter;
      final client = RestClient(dio, baseUrl: 'http://t', token: 'dev-token');
      final tools = await client.listTools();
      expect(tools, hasLength(1));
      expect(tools.first.name, 'echo');
      expect(tools.first.returnsA2ui, false);
      expect(adapter.calls.first.path, 'http://t/v1/tools');
      expect(adapter.calls.first.headers['Authorization'], 'Bearer dev-token');
    });

    test('invoke posts JSON body and parses trace_id', () async {
      final dio = Dio();
      final adapter = _RecordingAdapter();
      adapter.responder = (req) => ResponseBody.fromString(
            '{"trace_id":"t-1","result":{"ok":true}}',
            200,
            headers: {
              Headers.contentTypeHeader: ['application/json'],
            },
          );
      dio.httpClientAdapter = adapter;
      final client = RestClient(dio, baseUrl: 'http://t');
      final result = await client.invoke('echo', {'msg': 'hi'},
          a2uiVersion: '0.9');
      expect(result.statusCode, 200);
      expect(result.traceId, 't-1');
      expect(adapter.calls.first.method, 'POST');
      expect(adapter.calls.first.path, 'http://t/v1/tools/echo');
      expect(adapter.calls.first.headers['Accept'],
          'application/json+a2ui; version=0.9');
    });
  });

  group('McpClient', () {
    test('posts to the advertised endpoint — bare origin gets /, '
        'mount-path base is used as-is (single-port embed /mcp)', () async {
      // Port-mode: base is an origin, MCP lives at root → path `/`.
      final dioRoot = Dio()..httpClientAdapter = _RecordingAdapter();
      final atRoot = dioRoot.httpClientAdapter as _RecordingAdapter;
      await McpClient(dioRoot, baseUrl: 'http://t:8001').listTools();
      expect(atRoot.calls.first.path, 'http://t:8001/');

      // Single-port embed: `/v1/runtime` advertised mcp_base=/mcp, so the
      // base carries a mount path. Appending `/` would yield `/mcp/`,
      // which 404s against the nested router — post to `/mcp` exactly.
      final dioMount = Dio()..httpClientAdapter = _RecordingAdapter();
      final atMount = dioMount.httpClientAdapter as _RecordingAdapter;
      await McpClient(dioMount, baseUrl: 'http://t:8089/mcp').listTools();
      expect(atMount.calls.first.path, 'http://t:8089/mcp');

      // The editable Console envelope shows the same endpoint.
      expect(
        McpClient(Dio(), baseUrl: 'http://t:8089/mcp')
            .buildRequest('echo', const {})
            .url,
        'http://t:8089/mcp',
      );
    });

    test('listTools translates camelCase + x-triton extension', () async {
      final dio = Dio();
      final adapter = _RecordingAdapter();
      adapter.responder = (req) => ResponseBody.fromString(
            '{"jsonrpc":"2.0","id":1,"result":{"tools":'
            '[{"name":"echo","inputSchema":{"type":"object"},'
            '"x-triton":{"returns_a2ui":true}}]}}',
            200,
            headers: {
              Headers.contentTypeHeader: ['application/json'],
            },
          );
      dio.httpClientAdapter = adapter;
      final client = McpClient(dio, baseUrl: 'http://t');
      final tools = await client.listTools();
      expect(tools.first.name, 'echo');
      expect(tools.first.returnsA2ui, true);
      // JSON-RPC envelope check
      expect(adapter.calls.first.method, 'POST');
      expect(adapter.calls.first.data['method'], 'tools/list');
    });

    test('initialize negotiates protocol + parses serverInfo', () async {
      final dio = Dio();
      final adapter = _RecordingAdapter();
      adapter.responder = (req) => ResponseBody.fromString(
            '{"jsonrpc":"2.0","id":1,"result":{'
            '"protocolVersion":"2025-06-18",'
            '"capabilities":{"tools":{},"resources":{}},'
            '"serverInfo":{"name":"triton","version":"0.0.1"}}}',
            200,
            headers: {
              Headers.contentTypeHeader: ['application/json'],
            },
          );
      dio.httpClientAdapter = adapter;
      final client = McpClient(dio, baseUrl: 'http://t');
      final info = await client.initialize();
      expect(info.protocolVersion, '2025-06-18');
      expect(info.serverName, 'triton');
      expect(info.serverVersion, '0.0.1');
      expect(info.capabilities.keys, containsAll(['tools', 'resources']));
      expect(adapter.calls.first.data['method'], 'initialize');
      expect(adapter.calls.first.data['params']['protocolVersion'],
          '2025-06-18');
    });

    test('readResource parses contents[0]', () async {
      final dio = Dio();
      final adapter = _RecordingAdapter();
      adapter.responder = (req) => ResponseBody.fromString(
            '{"jsonrpc":"2.0","id":1,"result":{"contents":[{'
            '"uri":"ui://triton/runtime.html",'
            '"mimeType":"text/html","text":"<html></html>"}]}}',
            200,
            headers: {
              Headers.contentTypeHeader: ['application/json'],
            },
          );
      dio.httpClientAdapter = adapter;
      final client = McpClient(dio, baseUrl: 'http://t');
      final res = await client.readResource('ui://triton/runtime.html');
      expect(res.uri, 'ui://triton/runtime.html');
      expect(res.mimeType, 'text/html');
      expect(res.text, '<html></html>');
      expect(adapter.calls.first.data['method'], 'resources/read');
      expect(adapter.calls.first.data['params']['uri'],
          'ui://triton/runtime.html');
    });
  });

  group('A2aClient', () {
    test('invoke builds Message{parts:[{data:{tool,args}}], metadata}',
        () async {
      final dio = Dio();
      final adapter = _RecordingAdapter();
      adapter.responder = (req) => ResponseBody.fromString(
            '{"parts":[{"data":{"result":{"ok":true}}}],'
            '"metadata":{"trace_id":"t-2"}}',
            200,
            headers: {
              Headers.contentTypeHeader: ['application/json'],
            },
          );
      dio.httpClientAdapter = adapter;
      final client = A2aClient(dio, baseUrl: 'http://t');
      final r = await client.invoke('echo', {'msg': 'hi'},
          a2uiVersion: '0.8');
      expect(r.traceId, 't-2');
      final body = adapter.calls.first.data as Map<String, dynamic>;
      expect(body['parts'][0]['data']['tool'], 'echo');
      expect(body['metadata']['a2ui_version'], 'v0.8');
    });

    test('invoke surfaces metadata.task_state on success', () async {
      final dio = Dio();
      final adapter = _RecordingAdapter();
      adapter.responder = (req) => ResponseBody.fromString(
            '{"parts":[{"data":{"result":{"ok":true}}}],'
            '"metadata":{"trace_id":"t-3","task_state":"completed"}}',
            200,
            headers: {
              Headers.contentTypeHeader: ['application/json'],
            },
          );
      dio.httpClientAdapter = adapter;
      final client = A2aClient(dio, baseUrl: 'http://t');
      final r = await client.invoke('echo', const {});
      expect(r.taskState, 'completed');
    });
  });
}
