import 'package:flutter/material.dart';

import '../features/a2ui_diff/a2ui_diff_page.dart';
import '../features/audit/audit_page.dart';
import '../features/console/console_page.dart';
import '../features/dashboard/dashboard_page.dart';
import '../features/manifest/manifest_page.dart';
import '../features/metrics/metrics_page.dart';
import '../features/settings/settings_page.dart';
import '../features/trace/trace_page.dart';

/// Top-level navigation rail + IndexedStack — same pattern as
/// `heron/lib/ui/focus_layout.dart`. Wide screens get the rail;
/// narrow screens get a bottom navigation bar. No go_router yet —
/// the routes are simple enough that an enum-based switcher beats
/// the indirection. go_router lands in PR E2 when the playground's
/// deep-linkable per-tool URLs need it.
class AppShell extends StatefulWidget {
  const AppShell({super.key});

  @override
  State<AppShell> createState() => _AppShellState();
}

enum _Pane {
  dashboard,
  console,
  trace,
  a2uiDiff,
  manifest,
  audit,
  metrics,
  settings,
}

class _AppShellState extends State<AppShell> {
  _Pane _pane = _Pane.dashboard;

  static const _items = <_NavItem>[
    _NavItem(_Pane.dashboard, Icons.home_outlined, Icons.home, 'Dashboard'),
    _NavItem(_Pane.console, Icons.terminal_outlined, Icons.terminal, 'Console'),
    _NavItem(_Pane.trace, Icons.timeline_outlined, Icons.timeline, 'Trace'),
    _NavItem(
      _Pane.a2uiDiff,
      Icons.compare_outlined,
      Icons.compare,
      'A2UI diff',
    ),
    _NavItem(
      _Pane.manifest,
      Icons.description_outlined,
      Icons.description,
      'Manifest',
    ),
    _NavItem(_Pane.audit, Icons.list_alt_outlined, Icons.list_alt, 'Audit'),
    _NavItem(
      _Pane.metrics,
      Icons.query_stats_outlined,
      Icons.query_stats,
      'Metrics',
    ),
    _NavItem(
      _Pane.settings,
      Icons.settings_outlined,
      Icons.settings,
      'Settings',
    ),
  ];

  @override
  Widget build(BuildContext context) {
    final wide = MediaQuery.of(context).size.width >= 900;
    final body = IndexedStack(
      index: _pane.index,
      children: const [
        DashboardPage(),
        ConsolePage(),
        TracePage(),
        A2uiDiffPage(),
        ManifestPage(),
        AuditPage(),
        MetricsPage(),
        SettingsPage(),
      ],
    );
    if (wide) {
      return Scaffold(
        body: Row(
          children: [
            NavigationRail(
              selectedIndex: _pane.index,
              onDestinationSelected: (i) =>
                  setState(() => _pane = _Pane.values[i]),
              labelType: NavigationRailLabelType.all,
              destinations: [
                for (final item in _items)
                  NavigationRailDestination(
                    icon: Icon(item.icon),
                    selectedIcon: Icon(item.selectedIcon),
                    label: Text(item.label),
                  ),
              ],
            ),
            const VerticalDivider(width: 1),
            Expanded(child: body),
          ],
        ),
      );
    }
    return Scaffold(
      body: body,
      bottomNavigationBar: NavigationBar(
        selectedIndex: _pane.index,
        onDestinationSelected: (i) => setState(() => _pane = _Pane.values[i]),
        destinations: [
          for (final item in _items)
            NavigationDestination(
              icon: Icon(item.icon),
              selectedIcon: Icon(item.selectedIcon),
              label: item.label,
            ),
        ],
      ),
    );
  }
}

class _NavItem {
  const _NavItem(this.pane, this.icon, this.selectedIcon, this.label);
  final _Pane pane;
  final IconData icon;
  final IconData selectedIcon;
  final String label;
}
