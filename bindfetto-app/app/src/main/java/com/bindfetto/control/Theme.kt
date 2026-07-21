package com.bindfetto.control

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.isSpecified
import androidx.compose.ui.unit.sp

// Palette pulled from the app icon: cyan / blue / purple nodes on a dark navy field.
private val Cyan = Color(0xFF22D3EE)
private val Blue = Color(0xFF6B7FF0)
private val Purple = Color(0xFFB794F6)
private val NavyBg = Color(0xFF0B0D1A)
private val NavySurface = Color(0xFF14172B)
private val NavySurfaceHi = Color(0xFF1D2140)

private val BindfettoColors = darkColorScheme(
    primary = Cyan,
    onPrimary = Color(0xFF04121A),
    primaryContainer = Color(0xFF0E3A44),
    onPrimaryContainer = Color(0xFFA5F3FC),
    secondary = Blue,
    onSecondary = Color(0xFF0A1030),
    secondaryContainer = Color(0xFF283066),
    onSecondaryContainer = Color(0xFFD9DEFF),
    tertiary = Purple,
    onTertiary = Color(0xFF241033),
    tertiaryContainer = Color(0xFF3B2359),
    onTertiaryContainer = Color(0xFFEBD9FF),
    background = NavyBg,
    onBackground = Color(0xFFE6E8F2),
    surface = NavySurface,
    onSurface = Color(0xFFE6E8F2),
    surfaceVariant = NavySurfaceHi,
    onSurfaceVariant = Color(0xFFB6BCD8),
    outline = Color(0xFF3A4166),
    outlineVariant = Color(0xFF262B47),
)

// Scale every default Material 3 text style up for readability on-device.
private const val FontScale = 1.20f

private fun TextUnit.scaled(): TextUnit =
    if (isSpecified) (value * FontScale).sp else this

private fun TextStyle.scaled(): TextStyle =
    copy(fontSize = fontSize.scaled(), lineHeight = lineHeight.scaled())

private val BindfettoTypography = Typography().run {
    copy(
        displayLarge = displayLarge.scaled(),
        displayMedium = displayMedium.scaled(),
        displaySmall = displaySmall.scaled(),
        headlineLarge = headlineLarge.scaled(),
        headlineMedium = headlineMedium.scaled(),
        headlineSmall = headlineSmall.scaled(),
        titleLarge = titleLarge.scaled(),
        titleMedium = titleMedium.scaled(),
        titleSmall = titleSmall.scaled(),
        bodyLarge = bodyLarge.scaled(),
        bodyMedium = bodyMedium.scaled(),
        bodySmall = bodySmall.scaled(),
        labelLarge = labelLarge.scaled(),
        labelMedium = labelMedium.scaled(),
        labelSmall = labelSmall.scaled(),
    )
}

@Composable
fun BindfettoTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = BindfettoColors,
        typography = BindfettoTypography,
        content = content,
    )
}
