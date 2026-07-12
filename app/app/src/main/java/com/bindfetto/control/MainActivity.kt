package com.bindfetto.control

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.Checkbox
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Tab
import androidx.compose.material3.TabRow
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import androidx.lifecycle.viewmodel.compose.viewModel
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

enum class Tab { CONTROL, FILTER, DEPLOY }

data class UiState(
    val host: String = "127.0.0.1",
    val port: String = "3491",
    val advancedOpen: Boolean = false,
    val tab: Tab = Tab.CONTROL,
    val status: Map<String, String> = emptyMap(),
    val interfaces: List<String> = emptyList(),
    val selected: Set<String> = emptySet(),
    val deployLog: String = "Tap “Deploy & launch”. If the app lacks root it will print the adb fallback.",
    val message: String = "Not connected",
    val busy: Boolean = false,
)

/**
 * Drives the bindfetto runtime over its control channel. Interface discovery is only
 * enabled while the Filter tab is open (`TRACK on`/`off`), so the runtime carries no
 * discovery overhead the rest of the time.
 */
class ControlViewModel : ViewModel() {
    private val _state = mutableStateOf(UiState())
    val state: State<UiState> get() = _state

    private fun update(block: (UiState) -> UiState) { _state.value = block(_state.value) }

    fun setHost(v: String) = update { it.copy(host = v) }
    fun setPort(v: String) = update { it.copy(port = v.filter(Char::isDigit)) }
    fun toggleAdvanced() = update { it.copy(advancedOpen = !it.advancedOpen) }

    private fun client(): ControlClient? {
        val p = _state.value.port.toIntOrNull() ?: return null
        return ControlClient(_state.value.host.trim(), p)
    }

    /** Run a control action, funnelling errors into the status line. */
    private fun run(busyMsg: String, block: suspend (ControlClient) -> String) {
        val c = client() ?: run { update { it.copy(message = "Bad port") }; return }
        update { it.copy(busy = true, message = busyMsg) }
        viewModelScope.launch {
            try {
                val msg = block(c)
                update { it.copy(busy = false, message = msg) }
            } catch (e: Exception) {
                update { it.copy(busy = false, message = "Error: ${e.message}") }
            }
        }
    }

    // --- tab handling: gate discovery to the Filter tab ------------------------------
    fun selectTab(tab: Tab) {
        val prev = _state.value.tab
        update { it.copy(tab = tab) }
        if (tab == Tab.FILTER && prev != Tab.FILTER) startDiscovery()
        if (prev == Tab.FILTER && tab != Tab.FILTER) stopDiscovery()
        if (tab == Tab.CONTROL) refreshStatus()
    }

    // --- Control tab -----------------------------------------------------------------
    fun refreshStatus() = run("Loading status…") { c ->
        val s = c.status()
        update { it.copy(status = s) }
        // The status card shows the state; keep the message line to non-duplicative
        // connection feedback only.
        if (s.isEmpty()) "No response from ${_state.value.host}:${_state.value.port}"
        else "Connected · ${_state.value.host}:${_state.value.port}"
    }

    fun setCapturing(on: Boolean) = run(if (on) "Starting…" else "Stopping…") { c ->
        val r = c.setCapturing(on); refreshStatus(); r
    }

    fun setSink(mode: String) = run("Sink → $mode") { c ->
        val r = c.setSink(mode); refreshStatus(); r
    }

    fun setDlt(on: Boolean) = run("DLT → $on") { c ->
        val r = c.setDlt(on); refreshStatus(); r
    }

    fun setErrors(on: Boolean) = run("Errors → $on") { c ->
        val r = c.setErrors(on); refreshStatus(); r
    }

    fun setParcel(on: Boolean) = run("Parcel → $on") { c ->
        val r = c.setParcel(on); refreshStatus(); r
    }

    fun setParcelMax(bytes: Int) = run("Parcel cap → ${bytes}B") { c ->
        val r = c.setParcelMax(bytes); refreshStatus(); r
    }

    // --- Filter tab ------------------------------------------------------------------
    private fun startDiscovery() = run("Starting discovery…") { c ->
        c.setTracking(true)
        val observed = c.list()
        val active = c.activeFilter().toSet()
        update {
            it.copy(
                interfaces = (observed + active).distinct().sorted(),
                selected = active,
            )
        }
        "${observed.size} interfaces • ${active.size} in filter"
    }

    private fun stopDiscovery() {
        val c = client() ?: return
        viewModelScope.launch { runCatching { c.setTracking(false) } }
    }

    fun reload() = startDiscovery()

    fun toggle(iface: String) = update {
        val next = it.selected.toMutableSet()
        if (!next.add(iface)) next.remove(iface)
        it.copy(selected = next)
    }

    /** Add a manually-typed interface descriptor to the list and select it. */
    fun addInterface(raw: String) {
        val iface = raw.trim()
        if (iface.isEmpty()) return
        update {
            it.copy(
                interfaces = (it.interfaces + iface).distinct().sorted(),
                selected = it.selected + iface,
            )
        }
    }

    fun applyFilter() = run("Applying…") { c ->
        val sel = _state.value.selected.toList().sorted()
        "SET ${sel.size}: " + c.set(sel)
    }

    fun clearFilter() = run("Clearing…") { c ->
        val r = c.set(emptyList())
        update { it.copy(selected = emptySet()) }
        "CLEAR: $r"
    }

    // --- Deploy tab ------------------------------------------------------------------
    fun attemptDeploy(nativeLibDir: String) {
        update { it.copy(busy = true, deployLog = "Attempting deploy…") }
        val port = _state.value.port.ifBlank { "3491" }
        viewModelScope.launch {
            val log = withContext(Dispatchers.IO) { runDeploy(nativeLibDir, port) }
            update { it.copy(busy = false, deployLog = log) }
        }
    }

    private fun runDeploy(nativeLibDir: String, port: String): String {
        val bin = "$nativeLibDir/libbindfetto.so"
        val present = java.io.File(bin).exists()
        val sb = StringBuilder("bundled binary: $bin\n")
        sb.append(if (present) "  present\n\n" else "  MISSING — build the runtime, then rebuild the app\n\n")
        if (present) {
            // Detach the daemon (setsid + nohup, output to a log) so it outlives this `su`
            // process — a bare `&` would share su's process group and die when su exits,
            // leaving nothing on the control port for the app to connect to.
            val cmd = "cp $bin /data/local/tmp/bindfetto && chmod 755 /data/local/tmp/bindfetto && " +
                "setenforce 0 && setsid nohup /data/local/tmp/bindfetto --control $port --sink none " +
                "</dev/null >/data/local/tmp/bindfetto.log 2>&1 &"
            try {
                val p = Runtime.getRuntime().exec(arrayOf("su", "-c", cmd))
                val code = p.waitFor()
                if (code == 0) return sb.append("Deployed + launched via su (exit 0).").toString()
                sb.append("su exited $code — this app has no root/signature privilege.\n\n")
            } catch (e: Exception) {
                sb.append("su unavailable: ${e.message}\n\n")
            }
        }
        sb.append("Fallback — push + run from your bindfetto checkout:\n")
        sb.append("  adb push runtime/target/aarch64-linux-android/release/bindfetto /data/local/tmp/\n")
        sb.append("  adb shell 'setenforce 0; setsid nohup /data/local/tmp/bindfetto --control $port --sink none >/data/local/tmp/bindfetto.log 2>&1 &'\n")
        sb.append("  adb forward tcp:$port tcp:$port")
        return sb.toString()
    }
}

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            BindfettoTheme {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = MaterialTheme.colorScheme.background,
                ) { AppScreen() }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun AppScreen(vm: ControlViewModel = viewModel()) {
    val s by vm.state
    // Pull status once on launch so the app is usable without any "connect" step.
    LaunchedEffect(Unit) { vm.refreshStatus() }
    Scaffold(
        containerColor = MaterialTheme.colorScheme.background,
        topBar = {
            TopAppBar(
                title = {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Image(
                            painter = painterResource(R.drawable.bindfetto_logo),
                            contentDescription = null,
                            modifier = Modifier.size(28.dp).clip(RoundedCornerShape(7.dp)),
                        )
                        Spacer(Modifier.width(10.dp))
                        Text("bindfetto", fontWeight = FontWeight.SemiBold)
                    }
                },
                colors = TopAppBarDefaults.topAppBarColors(
                    containerColor = MaterialTheme.colorScheme.surface,
                    titleContentColor = MaterialTheme.colorScheme.onSurface,
                    actionIconContentColor = MaterialTheme.colorScheme.primary,
                ),
                actions = {
                    TextButton(onClick = vm::refreshStatus, enabled = !s.busy) { Text("Refresh") }
                    TextButton(onClick = vm::toggleAdvanced) {
                        Text(if (s.advancedOpen) "Advanced ▾" else "Advanced ▸")
                    }
                },
            )
        },
    ) { pad ->
        Column(modifier = Modifier.fillMaxSize().padding(pad)) {
            if (s.advancedOpen) ConnectionBar(s, vm)
            TabRow(selectedTabIndex = s.tab.ordinal) {
                Tab(selected = s.tab == Tab.CONTROL, onClick = { vm.selectTab(Tab.CONTROL) },
                    text = { Text("Control") })
                Tab(selected = s.tab == Tab.FILTER, onClick = { vm.selectTab(Tab.FILTER) },
                    text = { Text("Filter") })
                Tab(selected = s.tab == Tab.DEPLOY, onClick = { vm.selectTab(Tab.DEPLOY) },
                    text = { Text("Deploy") })
            }
            // Control + Filter tabs show the status inline (next to their action buttons)
            // to save vertical space; other tabs keep it here on top.
            if (s.tab != Tab.FILTER && s.tab != Tab.CONTROL) {
                Text(s.message, modifier = Modifier.padding(horizontal = 16.dp, vertical = 6.dp),
                    style = MaterialTheme.typography.bodyMedium)
            }
            when (s.tab) {
                Tab.CONTROL -> ControlTab(s, vm)
                Tab.FILTER -> FilterTab(s, vm)
                Tab.DEPLOY -> DeployTab(s, vm)
            }
        }
    }
}

@Composable
private fun ConnectionBar(s: UiState, vm: ControlViewModel) {
    // The app connects to bindfetto on the same device, so host is almost always
    // localhost; only the --control port ever varies. Hidden behind "Advanced".
    Row(
        modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        OutlinedTextField(s.host, vm::setHost, label = { Text("Host") },
            modifier = Modifier.width(180.dp), singleLine = true)
        Spacer(Modifier.width(8.dp))
        OutlinedTextField(s.port, vm::setPort, label = { Text("Port") },
            modifier = Modifier.width(96.dp), singleLine = true)
    }
}

@Composable
private fun ControlTab(s: UiState, vm: ControlViewModel) {
    val connected = s.status.isNotEmpty()
    val capturing = s.status["capturing"] == "on"
    val dltOn = s.status["dlt"] == "on"
    val errorsOn = s.status["errors"] == "on"
    val parcelOn = s.status["parcel"] == "on"
    val parcelMax = s.status["parcel_max"] ?: "256"
    val filterActive = (s.status["filter"]?.toIntOrNull() ?: 0) > 0
    val sink = s.status["sink"] ?: "console"
    Column(
        modifier = Modifier.fillMaxSize().padding(16.dp).verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        // Capture state pill + Start/Stop. The pill (not a duplicate key=value line) is
        // the single readout for capture; the counts card below shows the rest.
        Row(verticalAlignment = Alignment.CenterVertically) {
            // Status inline (was the top line), left of the capture pill — same size as it.
            Text(
                s.message,
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
            Spacer(Modifier.width(12.dp))
            StatusDot(
                color = when {
                    !connected -> MaterialTheme.colorScheme.outline
                    capturing -> MaterialTheme.colorScheme.primary
                    else -> MaterialTheme.colorScheme.tertiary
                }
            )
            Spacer(Modifier.width(8.dp))
            Text(
                if (!connected) "Not connected" else if (capturing) "Capturing" else "Paused",
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.SemiBold,
            )
            Spacer(Modifier.width(12.dp))
            Button(onClick = { vm.setCapturing(true) }, enabled = !s.busy && !capturing) { Text("Start") }
            Spacer(Modifier.width(8.dp))
            OutlinedButton(onClick = { vm.setCapturing(false) }, enabled = !s.busy && capturing) { Text("Stop") }
        }

        Section("Sink") {
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                listOf("console", "logcat", "both", "none").forEach { m ->
                    FilterChip(selected = sink == m, onClick = { vm.setSink(m) }, label = { Text(m) })
                }
            }
        }

        Section("DLT streaming") {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    if (dltOn) "Streaming to DLT (port ${s.status["dlt_port"] ?: "?"})" else "Off",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.weight(1f))
                Switch(checked = dltOn, onCheckedChange = { vm.setDlt(it) }, enabled = !s.busy)
            }
        }

        Section("Error capture") {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    if (errorsOn) "Reporting BR_FAILED/DEAD_REPLY" else "Off",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.weight(1f))
                Switch(checked = errorsOn, onCheckedChange = { vm.setErrors(it) }, enabled = !s.busy)
            }
        }

        ParcelSection(s, vm, parcelOn, parcelMax, filterActive)

        ElevatedCard {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text("Counts", style = MaterialTheme.typography.titleSmall,
                    color = MaterialTheme.colorScheme.primary)
                StatRow("Filter", s.status["filter"]?.let { "$it interfaces" } ?: "—")
                StatRow("Captured", s.status["captured"] ?: "—")
                StatRow("Emitted", s.status["emitted"] ?: "—")
            }
        }
    }
}

/**
 * Parcel payload capture (M6): a toggle (needs an active filter — the runtime refuses it
 * otherwise) plus a byte-cap editor. The cap is clamped to the 30 KiB ceiling by the
 * runtime; the input re-seeds from `parcel_max` whenever the status refreshes.
 */
@Composable
private fun ParcelSection(
    s: UiState,
    vm: ControlViewModel,
    parcelOn: Boolean,
    parcelMax: String,
    filterActive: Boolean,
) {
    Section("Parcel payload") {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                when {
                    !filterActive -> "Needs an active filter (set one in Filter)"
                    parcelOn -> "Capturing up to ${parcelMax}B/txn"
                    else -> "Off"
                },
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.weight(1f),
            )
            Spacer(Modifier.width(8.dp))
            Switch(
                checked = parcelOn,
                onCheckedChange = { vm.setParcel(it) },
                enabled = !s.busy && filterActive,
            )
        }
        // Re-seed the editor from the server's value on every status refresh.
        var capInput by remember(parcelMax) { mutableStateOf(parcelMax) }
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            OutlinedTextField(
                value = capInput,
                onValueChange = { capInput = it.filter(Char::isDigit).take(5) },
                label = { Text("Max bytes (≤ 30720)") },
                singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                modifier = Modifier.weight(1f),
            )
            Button(
                onClick = { capInput.toIntOrNull()?.let(vm::setParcelMax) },
                enabled = !s.busy && capInput.isNotBlank() && capInput != parcelMax,
            ) { Text("Set") }
        }
    }
}

@Composable
private fun StatusDot(color: androidx.compose.ui.graphics.Color) {
    Box(Modifier.size(12.dp).clip(RoundedCornerShape(6.dp)).background(color))
}

@Composable
private fun Section(title: String, content: @Composable () -> Unit) {
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text(title, style = MaterialTheme.typography.labelLarge,
            color = MaterialTheme.colorScheme.primary)
        content()
    }
}

@Composable
private fun StatRow(label: String, value: String) {
    Row(Modifier.fillMaxWidth()) {
        Text(label, style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.weight(1f))
        Text(value, style = MaterialTheme.typography.bodyMedium, fontFamily = FontFamily.Monospace)
    }
}

@Composable
private fun FilterTab(s: UiState, vm: ControlViewModel) {
    var query by remember { mutableStateOf("") }
    val q = query.trim()
    val shown = if (q.isEmpty()) s.interfaces
                else s.interfaces.filter { it.contains(q, ignoreCase = true) }
    // Show Add only for a query that isn't already an entry (so it doubles as "add custom").
    val canAdd = q.isNotEmpty() && s.interfaces.none { it.equals(q, ignoreCase = true) }
    Column(modifier = Modifier.fillMaxSize().padding(horizontal = 12.dp, vertical = 8.dp)) {
        // One field: filters the list live, and adds a not-yet-listed descriptor.
        OutlinedTextField(
            value = query,
            onValueChange = { query = it },
            label = { Text("Filter or add interface") },
            placeholder = { Text("android.os.IPowerManager") },
            singleLine = true,
            textStyle = LocalTextStyle.current.copy(fontFamily = FontFamily.Monospace),
            trailingIcon = {
                when {
                    canAdd -> TextButton(onClick = { vm.addInterface(q); query = "" }) { Text("Add") }
                    query.isNotEmpty() -> TextButton(onClick = { query = "" }) { Text("Clear") }
                }
            },
            modifier = Modifier.fillMaxWidth(),
        )
        Row(
            modifier = Modifier.fillMaxWidth().padding(vertical = 4.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                s.message,
                style = MaterialTheme.typography.bodySmall,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
            Button(onClick = vm::applyFilter, enabled = !s.busy) { Text("Apply") }
            OutlinedButton(onClick = vm::clearFilter, enabled = !s.busy) { Text("Clear") }
            TextButton(onClick = vm::reload, enabled = !s.busy) { Text("Reload") }
        }
        LazyColumn(modifier = Modifier.fillMaxSize()) {
            items(shown) { iface ->
                Row(modifier = Modifier.fillMaxWidth().padding(vertical = 2.dp),
                    verticalAlignment = Alignment.CenterVertically) {
                    Checkbox(checked = iface in s.selected, onCheckedChange = { vm.toggle(iface) })
                    Text(iface, fontFamily = FontFamily.Monospace,
                        style = MaterialTheme.typography.bodySmall)
                }
            }
        }
    }
}

@Composable
private fun DeployTab(s: UiState, vm: ControlViewModel) {
    val ctx = LocalContext.current
    Column(modifier = Modifier.fillMaxSize().padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        Text(
            "Deploys the bundled bindfetto binary and launches it as a --control daemon. " +
                "Needs root/signature privilege; otherwise it prints the adb commands to run yourself.",
            style = MaterialTheme.typography.bodyMedium,
        )
        Button(
            onClick = { vm.attemptDeploy(ctx.applicationInfo.nativeLibraryDir) },
            enabled = !s.busy,
        ) { Text("Deploy & launch") }
        Card {
            Text(
                s.deployLog,
                modifier = Modifier.padding(12.dp).verticalScroll(rememberScrollState()),
                fontFamily = FontFamily.Monospace,
                style = MaterialTheme.typography.bodySmall,
            )
        }
    }
}
