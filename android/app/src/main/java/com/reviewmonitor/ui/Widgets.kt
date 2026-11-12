package com.reviewmonitor.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/** 和电脑版 widgets.slint 的 Theme 同一套颜色 */
object C {
    val bg = Color(0xFF14161B)
    val panel = Color(0xFF1B1E25)
    val card = Color(0xFF22262F)
    val border = Color(0xFF2F3441)
    val text = Color(0xFFECEEF3)
    val sub = Color(0xFF9AA1B0)
    val faint = Color(0xFF6B7282)
    val accent = Color(0xFFFFC300)
    val accentInk = Color(0xFF1D1A10)
    val bad = Color(0xFFFF5F57)
    val warn = Color(0xFFFFA53D)
    val good = Color(0xFF39C98A)
    val mt = Color(0xFFFFC300)
    val dp = Color(0xFFFF7A45)
}

@Composable
fun Pill(text: String, modifier: Modifier = Modifier, primary: Boolean = false, enabled: Boolean = true, onClick: () -> Unit) {
    val bg = when { !enabled -> C.card; primary -> C.accent; else -> C.card }
    val fg = when { !enabled -> C.faint; primary -> C.accentInk; else -> C.text }
    Box(
        modifier
            .defaultMinSize(minHeight = 36.dp)
            .clip(RoundedCornerShape(18.dp))
            .background(bg)
            .border(1.dp, if (primary && enabled) Color.Transparent else C.border, RoundedCornerShape(18.dp))
            .clickable(enabled = enabled, onClick = onClick)
            .padding(horizontal = 16.dp, vertical = 8.dp),
        contentAlignment = Alignment.Center,
    ) { Text(text, color = fg, fontSize = 14.sp, fontWeight = if (primary) FontWeight.Bold else FontWeight.Medium) }
}

@Composable
fun Tag(text: String, tint: Color) {
    Box(
        Modifier.clip(RoundedCornerShape(6.dp)).background(tint.copy(alpha = 0.16f)).padding(horizontal = 7.dp, vertical = 2.dp),
    ) { Text(text, color = tint, fontSize = 12.sp, fontWeight = FontWeight.SemiBold) }
}

@Composable
fun Dot(color: Color, size: Int = 8) {
    Box(Modifier.size(size.dp).clip(CircleShape).background(color))
}

@Composable
fun Toggle(checked: Boolean, onChange: (Boolean) -> Unit) {
    Switch(
        checked = checked, onCheckedChange = onChange,
        colors = SwitchDefaults.colors(
            checkedThumbColor = C.accentInk, checkedTrackColor = C.accent,
            uncheckedThumbColor = C.sub, uncheckedTrackColor = C.card, uncheckedBorderColor = C.border,
        ),
    )
}

/** − 数值 + */
@Composable
fun Stepper(value: Float, min: Float, max: Float, step: Float, unit: String, decimals: Int = 0, onChange: (Float) -> Unit) {
    fun clamp(v: Float) = v.coerceIn(min, max)
    Row(verticalAlignment = Alignment.CenterVertically) {
        StepBtn("−", value > min) { onChange(clamp(value - step)) }
        Text(
            (if (decimals == 0) value.toInt().toString() else "%.${decimals}f".format(value)) + " " + unit,
            color = C.text, fontSize = 14.sp, modifier = Modifier.width(78.dp),
            textAlign = androidx.compose.ui.text.style.TextAlign.Center,
        )
        StepBtn("+", value < max) { onChange(clamp(value + step)) }
    }
}

@Composable
private fun StepBtn(t: String, enabled: Boolean, onClick: () -> Unit) {
    Box(
        Modifier.size(32.dp).clip(CircleShape).background(C.card).border(1.dp, C.border, CircleShape)
            .clickable(enabled = enabled, onClick = onClick),
        contentAlignment = Alignment.Center,
    ) { Text(t, color = if (enabled) C.text else C.faint, fontSize = 18.sp) }
}

@Composable
fun Section(title: String, content: @Composable () -> Unit) {
    Column(Modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text(title, color = C.sub, fontSize = 12.sp, fontWeight = FontWeight.Bold, letterSpacing = 1.sp)
        Column(
            Modifier.fillMaxWidth().clip(RoundedCornerShape(10.dp)).background(C.card).border(1.dp, C.border, RoundedCornerShape(10.dp))
                .padding(vertical = 4.dp),
        ) { content() }
    }
}

@Composable
fun SettingRow(label: String, hint: String = "", control: @Composable () -> Unit) {
    Row(
        Modifier.fillMaxWidth().defaultMinSize(minHeight = 58.dp).padding(horizontal = 16.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
            Text(label, color = C.text, fontSize = 14.sp)
            if (hint.isNotEmpty()) Text(hint, color = C.faint, fontSize = 12.sp, lineHeight = 16.sp)
        }
        Spacer(Modifier.width(12.dp))
        control()
    }
}

@Composable
fun Note(text: String, color: Color) {
    Box(
        Modifier.fillMaxWidth().clip(RoundedCornerShape(8.dp)).background(color.copy(alpha = 0.14f)).padding(horizontal = 14.dp, vertical = 9.dp),
    ) { Text(text, color = color, fontSize = 13.sp, lineHeight = 18.sp) }
}

@Composable
fun VSpace(h: Int) = Spacer(Modifier.height(h.dp))

/** 登录页的输入框 */
@Composable
fun Field(value: String, onChange: (String) -> Unit, label: String, enabled: Boolean = true, password: Boolean = false, numeric: Boolean = false, trailing: (@Composable () -> Unit)? = null) {
    androidx.compose.material3.OutlinedTextField(
        value = value, onValueChange = onChange, enabled = enabled, singleLine = true,
        label = { Text(label) },
        trailingIcon = trailing,
        visualTransformation = if (password) androidx.compose.ui.text.input.PasswordVisualTransformation() else androidx.compose.ui.text.input.VisualTransformation.None,
        keyboardOptions = androidx.compose.foundation.text.KeyboardOptions(
            keyboardType = if (numeric) androidx.compose.ui.text.input.KeyboardType.Number else if (password) androidx.compose.ui.text.input.KeyboardType.Password else androidx.compose.ui.text.input.KeyboardType.Ascii,
            autoCorrectEnabled = false,
        ),
        shape = RoundedCornerShape(10.dp),
        colors = androidx.compose.material3.OutlinedTextFieldDefaults.colors(
            focusedTextColor = C.text, unfocusedTextColor = C.text, disabledTextColor = C.sub,
            focusedContainerColor = C.card, unfocusedContainerColor = C.card, disabledContainerColor = C.card,
            focusedBorderColor = C.accent, unfocusedBorderColor = C.border, disabledBorderColor = C.border,
            focusedLabelColor = C.accent, unfocusedLabelColor = C.sub, disabledLabelColor = C.faint,
            cursorColor = C.accent,
        ),
        modifier = Modifier.fillMaxWidth(),
    )
}

@Composable
fun Check(checked: Boolean) {
    Box(
        Modifier.padding(top = 1.dp).size(18.dp).clip(RoundedCornerShape(4.dp))
            .background(if (checked) C.accent else C.card).border(1.dp, if (checked) C.accent else C.border, RoundedCornerShape(4.dp)),
        contentAlignment = Alignment.Center,
    ) { if (checked) Text("✓", color = C.accentInk, fontSize = 12.sp, fontWeight = FontWeight.Bold, lineHeight = 12.sp) }
}

@Composable
fun BigButton(text: String, enabled: Boolean = true, onClick: () -> Unit) {
    Box(
        Modifier.fillMaxWidth().height(50.dp).clip(RoundedCornerShape(10.dp))
            .background(if (enabled) C.accent else C.card)
            .clickable(enabled = enabled, onClick = onClick),
        contentAlignment = Alignment.Center,
    ) { Text(text, color = if (enabled) C.accentInk else C.faint, fontSize = 16.sp, fontWeight = FontWeight.Bold) }
}
