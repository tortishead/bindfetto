package com.bindfetto.control

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.Checkbox
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Tab
import androidx.compose.material3.TabRow
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
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
        if (s.isEmpty()) "No status" else "capturing=${s["capturing"]} • sink=${s["sink"]} • dlt=${s["dlt"]}"
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
            val cmd = "cp $bin /data/local/tmp/bindfetto && chmod 755 /data/local/tmp/bindfetto && " +
                "setenforce 0 && /data/local/tmp/bindfetto --control $port --sink none &"
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
        sb.append("  adb shell 'setenforce 0; /data/local/tmp/bindfetto --control $port --sink none &'\n")
        sb.append("  adb forward tcp:$port tcp:$port")
        return sb.toString()
    }
}

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                Surface(modifier = Modifier.fillMaxSize()) { AppScreen() }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun AppScreen(vm: ControlViewModel = viewModel()) {
    val s by vm.state
    Scaffold(topBar = { TopAppBar(title = { Text("bindfetto control") }) }) { pad ->
        Column(modifier = Modifier.fillMaxSize().padding(pad)) {
            ConnectionBar(s, vm)
            TabRow(selectedTabIndex = s.tab.ordinal) {
                Tab(selected = s.tab == Tab.CONTROL, onClick = { vm.selectTab(Tab.CONTROL) },
                    text = { Text("Control") })
                Tab(selected = s.tab == Tab.FILTER, onClick = { vm.selectTab(Tab.FILTER) },
                    text = { Text("Filter") })
                Tab(selected = s.tab == Tab.DEPLOY, onClick = { vm.selectTab(Tab.DEPLOY) },
                    text = { Text("Deploy") })
            }
            Text(s.message, modifier = Modifier.padding(horizontal = 16.dp, vertical = 6.dp),
                style = MaterialTheme.typography.bodyMedium)
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
    Row(
        modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        OutlinedTextField(s.host, vm::setHost, label = { Text("Host") },
            modifier = Modifier.width(180.dp), singleLine = true)
        Spacer(Modifier.width(8.dp))
        OutlinedTextField(s.port, vm::setPort, label = { Text("Port") },
            modifier = Modifier.width(96.dp), singleLine = true)
        Spacer(Modifier.width(8.dp))
        Button(onClick = vm::refreshStatus, enabled = !s.busy) { Text("Connect") }
    }
}

@Composable
private fun ControlTab(s: UiState, vm: ControlViewModel) {
    val capturing = s.status["capturing"] == "on"
    val dltOn = s.status["dlt"] == "on"
    val sink = s.status["sink"] ?: "?"
    Column(
        modifier = Modifier.fillMaxSize().padding(16.dp).verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Card {
            Column(Modifier.padding(12.dp)) {
                Text("Status", style = MaterialTheme.typography.titleMedium)
                if (s.status.isEmpty()) Text("— tap Connect —")
                else s.status.forEach { (k, v) ->
                    Text("$k = $v", fontFamily = FontFamily.Monospace,
                        style = MaterialTheme.typography.bodySmall)
                }
            }
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = { vm.setCapturing(true) }, enabled = !s.busy && !capturing) { Text("Start") }
            OutlinedButton(onClick = { vm.setCapturing(false) }, enabled = !s.busy && capturing) { Text("Stop") }
        }
        Text("Sink", style = MaterialTheme.typography.titleSmall)
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            listOf("console", "logcat", "both", "none").forEach { m ->
                FilterChip(selected = sink == m, onClick = { vm.setSink(m) }, label = { Text(m) })
            }
        }
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text("DLT streaming")
            Spacer(Modifier.width(12.dp))
            Switch(checked = dltOn, onCheckedChange = { vm.setDlt(it) }, enabled = !s.busy)
        }
    }
}

@Composable
private fun FilterTab(s: UiState, vm: ControlViewModel) {
    Column(modifier = Modifier.fillMaxSize().padding(16.dp)) {
        Row(modifier = Modifier.padding(bottom = 8.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = vm::reload, enabled = !s.busy) { Text("Reload") }
            Button(onClick = vm::applyFilter, enabled = !s.busy) { Text("Apply filter") }
            OutlinedButton(onClick = vm::clearFilter, enabled = !s.busy) { Text("Clear") }
        }
        LazyColumn(modifier = Modifier.fillMaxSize()) {
            items(s.interfaces) { iface ->
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
