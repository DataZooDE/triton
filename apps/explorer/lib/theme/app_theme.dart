import 'package:flutter/material.dart';
import 'package:google_fonts/google_fonts.dart';

/// Material 3 theme copied from the heron Flutter app
/// (`/home/jr/Projects/datazoo/heron/lib/theme/app_theme.dart`) so the
/// explorer reads as native to the DataZoo Flutter portfolio. Color
/// tokens are the deep-teal palette extracted from the heron mockups;
/// typography uses Inter via `google_fonts`.
///
/// Heron's theme is `publish_to: 'none'` so we can't depend on it as a
/// package; copying the tokens is the lightest-touch reuse strategy
/// until the team chooses to extract a shared design-tokens package.
class ExplorerTheme {
  ExplorerTheme._();

  // Color tokens — same RGB values as heron.
  static const primary = Color(0xFF004149);
  static const primaryContainer = Color(0xFF1D5962);
  static const onPrimary = Color(0xFFFFFFFF);
  static const onPrimaryContainer = Color(0xFF96CED8);

  static const secondary = Color(0xFF006A61);
  static const secondaryContainer = Color(0xFF8FF1E2);
  static const onSecondary = Color(0xFFFFFFFF);
  static const onSecondaryContainer = Color(0xFF006F65);

  static const tertiary = Color(0xFF2A3C4E);
  static const tertiaryContainer = Color(0xFF415366);
  static const onTertiary = Color(0xFFFFFFFF);
  static const onTertiaryContainer = Color(0xFFB4C6DD);

  static const error = Color(0xFFBA1A1A);
  static const errorContainer = Color(0xFFFFDAD6);
  static const onError = Color(0xFFFFFFFF);
  static const onErrorContainer = Color(0xFF93000A);

  static const surface = Color(0xFFF7F9FF);
  static const surfaceContainerLow = Color(0xFFEDF4FF);
  static const surfaceContainer = Color(0xFFE3EFFF);
  static const surfaceContainerHigh = Color(0xFFD9EAFF);

  static const onSurface = Color(0xFF091D2E);
  static const onSurfaceVariant = Color(0xFF40484A);
  static const outline = Color(0xFF70797A);
  static const outlineVariant = Color(0xFFBFC8CA);

  /// Glassmorphism panel — heron uses this as a card background to
  /// soften dense data displays. The explorer reuses it for adapter
  /// response cards and tool result panes.
  static BoxDecoration glassPanel({double opacity = 0.85}) => BoxDecoration(
        color: Colors.white.withValues(alpha: opacity),
        borderRadius: BorderRadius.circular(12),
        boxShadow: [
          BoxShadow(
            color: Colors.black.withValues(alpha: 0.03),
            blurRadius: 1,
          ),
        ],
      );

  static ThemeData light() {
    const colorScheme = ColorScheme(
      brightness: Brightness.light,
      primary: primary,
      onPrimary: onPrimary,
      primaryContainer: primaryContainer,
      onPrimaryContainer: onPrimaryContainer,
      secondary: secondary,
      onSecondary: onSecondary,
      secondaryContainer: secondaryContainer,
      onSecondaryContainer: onSecondaryContainer,
      tertiary: tertiary,
      onTertiary: onTertiary,
      tertiaryContainer: tertiaryContainer,
      onTertiaryContainer: onTertiaryContainer,
      error: error,
      onError: onError,
      errorContainer: errorContainer,
      onErrorContainer: onErrorContainer,
      surface: surface,
      onSurface: onSurface,
      onSurfaceVariant: onSurfaceVariant,
      outline: outline,
      outlineVariant: outlineVariant,
    );

    return ThemeData(
      useMaterial3: true,
      colorScheme: colorScheme,
      textTheme: GoogleFonts.interTextTheme(),
      cardTheme: CardThemeData(
        elevation: 0,
        color: surfaceContainerLow,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(12),
        ),
      ),
      filledButtonTheme: FilledButtonThemeData(
        style: FilledButton.styleFrom(
          padding: const EdgeInsets.symmetric(horizontal: 24, vertical: 12),
        ),
      ),
      chipTheme: ChipThemeData(
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(20),
        ),
      ),
    );
  }
}
