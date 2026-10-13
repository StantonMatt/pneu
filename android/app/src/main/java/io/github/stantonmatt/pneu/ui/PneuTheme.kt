package io.github.stantonmatt.pneu.ui

import android.provider.Settings
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.*

@Immutable
data class PneuColors(val dark:Boolean, val bg:Color, val surface:Color, val raised:Color,
    val fill:Color, val fill2:Color, val separator:Color, val hairline:Color,
    val text:Color, val text2:Color, val text3:Color, val accent:Color=Color(0xFF4F5BFF),
    val onAccent:Color=Color.White, val accentText:Color, val accentSoft:Color,
    val onAccentSoft:Color, val warn:Color, val warnSoft:Color, val danger:Color,
    val dangerFill:Color=Color(0xFFD93D42), val dangerSoft:Color,
    val spark:Color=Color(0xFFFFB547), val switchOff:Color, val inverse:Color,
    val onInverse:Color, val scrim:Color, val linkStart:Color=Color(0xFF7381FF))
private val light=PneuColors(false,Color(0xFFF2F3F6),Color.White,Color.White,
    Color(0xFFEEF0F4),Color(0xFFE3E6EC),Color(0xFFE8EAEF),Color.Transparent,
    Color(0xFF0D0F14),Color(0xFF5C6270),Color(0xFF8B91A0),accentText=Color(0xFF3D47E0),
    accentSoft=Color(0xFFECEEFF),onAccentSoft=Color(0xFF2F38C8),warn=Color(0xFFB45309),
    warnSoft=Color(0xFFFFF1DC),danger=Color(0xFFD63B3B),dangerSoft=Color(0xFFFDECEC),
    switchOff=Color(0xFFD9DCE3),inverse=Color(0xFF1C1D22),onInverse=Color(0xFFF3F4F6),scrim=Color(0xFF0D0F14).copy(alpha=.38f))
private val dark=PneuColors(true,Color.Black,Color(0xFF141519),Color(0xFF1C1D22),
    Color(0xFF22242A),Color(0xFF2C2F36),Color(0xFF23252B),Color.White.copy(alpha=.06f),
    Color(0xFFF3F4F6),Color(0xFFA0A5B1),Color(0xFF7A7F8C),accentText=Color(0xFF9AA3FF),
    accentSoft=Color(0xFF1D2046),onAccentSoft=Color(0xFFC7CCFF),warn=Color(0xFFFFB547),
    warnSoft=Color(0xFF2D2210),danger=Color(0xFFFF7A7A),dangerSoft=Color(0xFF331618),
    switchOff=Color(0xFF3A3D45),inverse=Color(0xFFF3F4F6),onInverse=Color(0xFF0D0F14),scrim=Color.Black.copy(alpha=.62f))
val LocalPneuColors=staticCompositionLocalOf {light}
val LocalPneuMotion=staticCompositionLocalOf {true}
object PneuMotion {
    const val fast=120; const val short=200; const val medium=280; const val long=400
    val ease=CubicBezierEasing(.2f,0f,0f,1f)
    val easeOut=CubicBezierEasing(.05f,.7f,.1f,1f)
    val easeIn=CubicBezierEasing(.3f,0f,.8f,.15f)
    val transit=CubicBezierEasing(.45f,0f,.2f,1f)
}
object PneuSpace {val gutter=16.dp;val grid=4.dp;val row=56.dp;val twoLineRow=68.dp;val permissionRow=60.dp}
object PneuElevation {val card=0.dp;val menu=8.dp;val dialog=24.dp}
object PneuType {
    private fun style(size:Int,line:Int,weight:FontWeight=FontWeight.Normal,tracking:Float=0f)=TextStyle(fontFamily=FontFamily.Default,fontSize=size.sp,lineHeight=line.sp,fontWeight=weight,letterSpacing=tracking.sp)
    val largeTitle=style(32,38,FontWeight.SemiBold,-.6f)
    val title1=style(26,32,FontWeight.SemiBold,-.5f)
    val title2=style(20,26,FontWeight.SemiBold,-.3f)
    val headline=style(17,22,FontWeight.SemiBold)
    val body=style(16,22);val bodyStrong=style(15,20,FontWeight.Medium)
    val button=style(15,20,FontWeight.SemiBold);val smallButton=style(14,18,FontWeight.SemiBold)
    val subhead=style(14,19);val dialog=style(15,21);val section=style(13,18,FontWeight.SemiBold);val caption=style(12,16)
}
@Composable fun PneuTheme(dark:Boolean=isSystemInDarkTheme(),content:@Composable ()->Unit) {
    val c=if(dark) io.github.stantonmatt.pneu.ui.dark else light
    val context=LocalContext.current
    // Re-read on composition after returning from the system animation settings.
    val animate=Settings.Global.getFloat(context.contentResolver,Settings.Global.ANIMATOR_DURATION_SCALE,1f)>0f
    val scheme=(if(dark) darkColorScheme() else lightColorScheme()).copy(primary=c.accent,onPrimary=c.onAccent,
        primaryContainer=c.accentSoft,onPrimaryContainer=c.onAccentSoft,secondary=c.accentText,
        secondaryContainer=c.fill,onSecondaryContainer=c.text,tertiary=c.warn,tertiaryContainer=c.warnSoft,
        onTertiaryContainer=c.warn,background=c.bg,onBackground=c.text,surface=c.surface,onSurface=c.text,
        surfaceVariant=c.fill,onSurfaceVariant=c.text2,surfaceContainer=c.bg,surfaceContainerLow=c.surface,
        surfaceContainerLowest=c.surface,surfaceContainerHigh=c.raised,surfaceContainerHighest=c.fill2,
        error=c.danger,errorContainer=c.dangerSoft,onErrorContainer=c.danger,outline=c.fill2,outlineVariant=c.separator,
        inverseSurface=c.inverse,inverseOnSurface=c.onInverse,scrim=c.scrim)
    CompositionLocalProvider(LocalPneuColors provides c,LocalPneuMotion provides animate) {
        MaterialTheme(colorScheme=scheme,typography=Typography(headlineLarge=PneuType.largeTitle,headlineSmall=PneuType.title1,
            titleLarge=PneuType.title2,titleMedium=PneuType.headline,titleSmall=PneuType.section,bodyLarge=PneuType.body,
            bodyMedium=PneuType.subhead,bodySmall=PneuType.caption,labelLarge=PneuType.button),
            shapes=Shapes(small=RoundedCornerShape(10.dp),medium=RoundedCornerShape(14.dp),large=RoundedCornerShape(24.dp),extraLarge=RoundedCornerShape(28.dp)),content=content)
    }
}
