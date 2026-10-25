// Scratch-only diagnostic fixture. Never linked into or injected into the driver.
using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

namespace EdgeForegroundDiagnostic
{
    public sealed class NativeForeground : IDisposable
    {
        [StructLayout(LayoutKind.Sequential)] struct Point { public int x, y; }
        [StructLayout(LayoutKind.Sequential)] struct Rect { public int left, top, right, bottom; }
        [StructLayout(LayoutKind.Sequential)] struct Msg
        {
            public IntPtr hwnd; public uint message; public UIntPtr wParam; public IntPtr lParam;
            public uint time; public Point point; public uint privateValue;
        }
        [StructLayout(LayoutKind.Sequential)] struct GuiInfo
        {
            public uint size, flags;
            public IntPtr active, focus, capture, menuOwner, moveSize, caret;
            public Rect caretRect;
        }
        [StructLayout(LayoutKind.Sequential)] struct LastInput { public uint size, tick; }
        delegate void WinEvent(IntPtr hook, uint ev, IntPtr hwnd, int obj, int child, uint tid, uint time);
        delegate bool EnumWindow(IntPtr hwnd, IntPtr state);
        [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern IntPtr CreateWindowExW(uint ex, string cls, string title, uint style,
            int x, int y, int width, int height, IntPtr parent, IntPtr menu, IntPtr instance, IntPtr param);
        [DllImport("user32.dll")] static extern bool DestroyWindow(IntPtr hwnd);
        [DllImport("user32.dll")] static extern int GetMessageW(out Msg msg, IntPtr hwnd, uint min, uint max);
        [DllImport("user32.dll")] static extern bool TranslateMessage(ref Msg msg);
        [DllImport("user32.dll")] static extern IntPtr DispatchMessageW(ref Msg msg);
        [DllImport("user32.dll", SetLastError = true)] static extern bool PostThreadMessageW(uint tid, uint msg, UIntPtr wp, IntPtr lp);
        [DllImport("user32.dll")] static extern UIntPtr SetTimer(IntPtr hwnd, UIntPtr id, uint ms, IntPtr callback);
        [DllImport("user32.dll")] static extern bool KillTimer(IntPtr hwnd, UIntPtr id);
        [DllImport("user32.dll")] static extern IntPtr SetWinEventHook(uint min, uint max, IntPtr module, WinEvent callback, uint pid, uint tid, uint flags);
        [DllImport("user32.dll")] static extern bool UnhookWinEvent(IntPtr hook);
        [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
        [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowTextW(IntPtr hwnd, StringBuilder text, int count);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassNameW(IntPtr hwnd, StringBuilder text, int count);
        [DllImport("user32.dll")] static extern bool IsWindow(IntPtr hwnd);
        [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hwnd);
        [DllImport("user32.dll")] static extern IntPtr GetWindow(IntPtr hwnd, uint command);
        [DllImport("user32.dll", SetLastError = true)] static extern bool GetGUIThreadInfo(uint tid, ref GuiInfo info);
        [DllImport("user32.dll", SetLastError = true)] static extern bool SetForegroundWindow(IntPtr hwnd);
        [DllImport("user32.dll", SetLastError = true)] static extern bool AllowSetForegroundWindow(uint pid);
        [DllImport("user32.dll", SetLastError = true)] static extern IntPtr SendMessageTimeoutW(IntPtr hwnd, uint msg, UIntPtr wp, IntPtr lp, uint flags, uint ms, out UIntPtr result);
        [DllImport("user32.dll")] static extern bool EnumWindows(EnumWindow callback, IntPtr state);
        [DllImport("user32.dll")] static extern bool GetLastInputInfo(ref LastInput input);
        [DllImport("user32.dll", EntryPoint = "SystemParametersInfoW", SetLastError = true)]
        static extern bool SpiGet(uint action, uint param, ref uint value, uint flags);
        [DllImport("user32.dll", EntryPoint = "SystemParametersInfoW", SetLastError = true)]
        static extern bool SpiSet(uint action, uint param, IntPtr value, uint flags);
        [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
        [DllImport("kernel32.dll")] static extern void SetLastError(uint error);
        [DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        static extern bool QueryFullProcessImageNameW(IntPtr process, uint flags, StringBuilder text, ref uint count);
        [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);

        readonly object gate = new object();
        readonly StreamWriter output;
        readonly Stopwatch clock = Stopwatch.StartNew();
        readonly ManualResetEventSlim ready = new ManualResetEventSlim();
        readonly Thread pump;
        Exception startupError;
        IntPtr hwnd;
        uint threadId;
        volatile string phase = "startup";
        readonly WinEvent callback;
        public long Hwnd { get { return hwnd.ToInt64(); } }
        public int Pid { get { return Environment.ProcessId; } }

        public NativeForeground(string path)
        {
            output = new StreamWriter(path, false, new UTF8Encoding(false)) { AutoFlush = true };
            callback = OnEvent; // Retain delegates through UnhookWinEvent.
            pump = new Thread(Pump) { IsBackground = true, Name = "foreground-native-fixture" };
            pump.SetApartmentState(ApartmentState.STA);
            pump.Start();
            bool signaled = ready.Wait(5000);
            if (!signaled || startupError != null)
            {
                PostThreadMessageW(threadId, 0x12, UIntPtr.Zero, IntPtr.Zero);
                if (pump.Join(3000)) { output.Dispose(); ready.Dispose(); }
                if (!signaled) throw new TimeoutException("Native fixture readiness");
                throw new InvalidOperationException("Native fixture startup", startupError);
            }
        }
        public void Mark(string value) { phase = value; Log("phase", value); }
        void Log(string kind, object data)
        {
            lock (gate)
                output.WriteLine(JsonSerializer.Serialize(new { utc = DateTime.UtcNow.ToString("O"),
                    elapsed_ms = clock.Elapsed.TotalMilliseconds, phase, kind, data }));
        }
        void Pump()
        {
            IntPtr foregroundHook = IntPtr.Zero, windowHook = IntPtr.Zero;
            UIntPtr timer = UIntPtr.Zero;
            try
            {
                threadId = GetCurrentThreadId();
                // Built-in STATIC class, independent pumping thread, same shape as the native testkit fixture.
                hwnd = CreateWindowExW(0, "STATIC", "Cua Edge foreground diagnostic fixture",
                    0x10CF0000, 100, 100, 480, 240, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero);
                if (hwnd == IntPtr.Zero) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
                foregroundHook = SetWinEventHook(3, 3, IntPtr.Zero, callback, 0, 0, 0);
                windowHook = SetWinEventHook(0x8000, 0x800C, IntPtr.Zero, callback, 0, 0, 0);
                timer = SetTimer(IntPtr.Zero, UIntPtr.Zero, 100, IntPtr.Zero);
                if (foregroundHook == IntPtr.Zero || windowHook == IntPtr.Zero || timer == UIntPtr.Zero)
                    throw new InvalidOperationException("WinEvent hook/timer installation failed");
                ready.Set();
                int rc;
                while ((rc = GetMessageW(out Msg msg, IntPtr.Zero, 0, 0)) > 0)
                {
                    if (msg.message == 0x113) Log("foreground-poll", Window(GetForegroundWindow()));
                    TranslateMessage(ref msg);
                    DispatchMessageW(ref msg);
                }
                if (rc < 0) Log("pump-error", "GetMessageW returned -1");
            }
            catch (Exception error) { startupError = error; Log("pump-error", error.ToString()); ready.Set(); }
            finally
            {
                if (foregroundHook != IntPtr.Zero) UnhookWinEvent(foregroundHook);
                if (windowHook != IntPtr.Zero) UnhookWinEvent(windowHook);
                if (timer != UIntPtr.Zero) KillTimer(IntPtr.Zero, timer);
                if (hwnd != IntPtr.Zero) DestroyWindow(hwnd);
            }
        }
        void OnEvent(IntPtr hook, uint ev, IntPtr target, int obj, int child, uint tid, uint time)
        {
            // Top-level window lifecycle/name changes plus every foreground transition.
            if (ev != 3 && (obj != 0 || child != 0)) return;
            try { Log("winevent", new { ev, native_tick_ms = time, event_thread_id = tid,
                window = Window(target), foreground = Window(GetForegroundWindow()) }); }
            catch (Exception error) { Log("event-error", error.ToString()); }
        }
        public static object Window(IntPtr target)
        {
            uint tid = GetWindowThreadProcessId(target, out uint pid);
            var title = new StringBuilder(1024); var cls = new StringBuilder(256);
            GetWindowTextW(target, title, title.Capacity); GetClassNameW(target, cls, cls.Capacity);
            var info = new GuiInfo { size = (uint)Marshal.SizeOf<GuiInfo>() };
            bool guiOk = tid != 0 && GetGUIThreadInfo(tid, ref info);
            string image = null;
            IntPtr process = OpenProcess(0x1000, false, pid);
            if (process != IntPtr.Zero)
            {
                try { var path = new StringBuilder(32768); uint n = (uint)path.Capacity;
                    if (QueryFullProcessImageNameW(process, 0, path, ref n)) image = path.ToString(); }
                finally { CloseHandle(process); }
            }
            return new { hwnd = target.ToInt64(), pid, tid, title = title.ToString(), class_name = cls.ToString(),
                image, alive = IsWindow(target), visible = IsWindowVisible(target), owner_hwnd = GetWindow(target, 4).ToInt64(),
                gui_info_available = guiOk, gui_flags = info.flags, active_hwnd = info.active.ToInt64(),
                focus_hwnd = info.focus.ToInt64(), menu_owner_hwnd = info.menuOwner.ToInt64() };
        }
        public object[] Windows()
        {
            var rows = new List<object>();
            EnumWindow visitor = (target, unused) => { if (IsWindowVisible(target)) rows.Add(Window(target)); return true; };
            if (!EnumWindows(visitor, IntPtr.Zero)) throw new InvalidOperationException("EnumWindows failed");
            GC.KeepAlive(visitor);
            return rows.ToArray();
        }
        public bool IsFixtureForeground()
        {
            IntPtr current = GetForegroundWindow(); GetWindowThreadProcessId(current, out uint pid);
            return current == hwnd && pid == (uint)Pid && IsWindow(hwnd);
        }
        public object Snapshot()
        {
            SetLastError(0);
            bool responsive = SendMessageTimeoutW(hwnd, 0, UIntPtr.Zero, IntPtr.Zero, 2, 250, out _) != IntPtr.Zero;
            int pingError = Marshal.GetLastWin32Error();
            var input = new LastInput { size = (uint)Marshal.SizeOf<LastInput>() };
            bool inputOk = GetLastInputInfo(ref input);
            var value = new { foreground = Window(GetForegroundWindow()), fixture = Window(hwnd),
                fixture_is_foreground = IsFixtureForeground(), fixture_responsive = responsive,
                fixture_ping_error = pingError, last_input_available = inputOk, last_input_tick = input.tick,
                current_tick = Environment.TickCount64 };
            Log("snapshot", value); return value;
        }
        public object ActivateFixture(string reason)
        {
            object before = Window(GetForegroundWindow());
            SetLastError(0);
            bool requested = SetForegroundWindow(hwnd);
            int error = Marshal.GetLastWin32Error();
            object immediate = Window(GetForegroundWindow());
            Thread.Sleep(100);
            var value = new { reason, caller_pid = Pid, caller_thread_id = GetCurrentThreadId(),
                target_hwnd = Hwnd, requested, last_error = error,
                last_error_note = "SetForegroundWindow does not document extended error; zero is not proof of permission.",
                before, immediate, after_100ms = Window(GetForegroundWindow()), confirmed = IsFixtureForeground() };
            Log("harness-SetForegroundWindow-not-driver", value); return value;
        }
        public object Grant(uint daemonPid)
        {
            if (!IsFixtureForeground()) throw new InvalidOperationException("ASFW refused: fixture not foreground");
            SetLastError(0); bool allowed = AllowSetForegroundWindow(daemonPid); int error = Marshal.GetLastWin32Error();
            var value = new { daemon_pid = daemonPid, allowed, last_error = error, foreground = Window(GetForegroundWindow()) };
            Log("AllowSetForegroundWindow-exact-daemon", value); return value;
        }
        public static uint LockTimeout()
        {
            uint value = 0;
            if (!SpiGet(0x2000, 0, ref value, 0)) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
            return value;
        }
        public object SetLockTimeout(uint value)
        {
            uint before = LockTimeout(); SetLastError(0);
            bool requested = SpiSet(0x2001, 0, new IntPtr(value), 2); int error = Marshal.GetLastWin32Error();
            var result = new { before, value, requested, last_error = error, readback = LockTimeout(), persisted = false };
            Log("SystemParametersInfo-foreground-lock-timeout", result); return result;
        }
        public void Dispose()
        {
            bool posted = PostThreadMessageW(threadId, 0x12, UIntPtr.Zero, IntPtr.Zero);
            bool joined = pump.Join(3000);
            Log("fixture-cleanup", new { posted, joined, hwnd_still_alive = IsWindow(hwnd) });
            if (!joined) throw new TimeoutException("Native fixture pump did not stop");
            output.Dispose(); ready.Dispose();
        }
    }

    // Real process stdio transport; reader tasks never call PowerShell scriptblocks.
    public sealed class LineProcess : IDisposable
    {
        readonly Process process;
        readonly ConcurrentQueue<string> lines = new ConcurrentQueue<string>();
        readonly Task stdout, stderr;
        public int Pid { get { return process.Id; } }
        public bool Exited { get { return process.HasExited; } }
        public static string ProbePipe(string path)
        {
            // Same exchange as testkit daemon.rs. Cancel/dispose the actual pipe on timeout.
            using var pipe = new System.IO.Pipes.NamedPipeClientStream(".", path.Substring(9),
                System.IO.Pipes.PipeDirection.InOut, System.IO.Pipes.PipeOptions.Asynchronous);
            using var cancel = new CancellationTokenSource(1000);
            try
            {
                pipe.ConnectAsync(cancel.Token).GetAwaiter().GetResult();
                byte[] request = Encoding.UTF8.GetBytes("{\"method\":\"list\"}\n");
                pipe.WriteAsync(request, 0, request.Length, cancel.Token).GetAwaiter().GetResult();
                using var reader = new StreamReader(pipe, new UTF8Encoding(false));
                return reader.ReadLineAsync(cancel.Token).AsTask().GetAwaiter().GetResult();
            }
            catch (OperationCanceledException) { return null; }
            catch (IOException) { return null; }
        }
        public LineProcess(string exe, string[] arguments, string stateRoot, string logPrefix, bool unrestricted)
        {
            var start = new ProcessStartInfo(exe) { UseShellExecute = false, CreateNoWindow = true,
                RedirectStandardInput = true, RedirectStandardOutput = true, RedirectStandardError = true };
            foreach (string arg in arguments) start.ArgumentList.Add(arg);
            // Match testkit host_state.rs: preserve Windows shell discovery known folders.
            foreach (string key in new List<string>(start.Environment.Keys))
                if (key.StartsWith("CUA_", StringComparison.OrdinalIgnoreCase) || key.StartsWith("HERMES_", StringComparison.OrdinalIgnoreCase))
                    start.Environment.Remove(key);
            start.Environment["HOME"] = stateRoot;
            start.Environment["CUA_DRIVER_RS_HOME"] = Path.Combine(stateRoot, ".cua-driver");
            start.Environment["CUA_DRIVER_RS_TELEMETRY_ENABLED"] = "false";
            if (unrestricted)
            {
                start.Environment["CUA_DRIVER_PERMISSION_MODE"] = "unrestricted";
                start.Environment["CUA_DRIVER_DANGEROUSLY_BYPASS_APPROVALS"] = "1";
            }
            process = Process.Start(start);
            stdout = Task.Run(async () => {
                using var log = new StreamWriter(logPrefix + ".stdout.jsonl", false, new UTF8Encoding(false)) { AutoFlush = true };
                string line;
                while ((line = await process.StandardOutput.ReadLineAsync()) != null)
                {
                    log.WriteLine(JsonSerializer.Serialize(new { utc = DateTime.UtcNow.ToString("O"), line }));
                    lines.Enqueue(line);
                }
            });
            stderr = Task.Run(async () => {
                using var log = new StreamWriter(logPrefix + ".stderr.log", false, new UTF8Encoding(false)) { AutoFlush = true };
                string line;
                while ((line = await process.StandardError.ReadLineAsync()) != null)
                    log.WriteLine(DateTime.UtcNow.ToString("O") + " " + line);
            });
        }
        public void Send(string line) { process.StandardInput.WriteLine(line); process.StandardInput.Flush(); }
        public string Receive(int timeoutMs)
        {
            var clock = Stopwatch.StartNew();
            do
            {
                if (lines.TryDequeue(out string line)) return line;
                if (stdout.IsCompleted) throw new EndOfStreamException("Driver stdout closed");
                Thread.Sleep(5);
            } while (clock.ElapsedMilliseconds < timeoutMs);
            throw new TimeoutException("Driver response deadline; no launch auto-retry is performed");
        }
        public void Dispose()
        {
            process.StandardInput.Close();
            if (!process.WaitForExit(1000)) process.Kill(true);
            if (!process.WaitForExit(5000)) throw new TimeoutException("Owned driver process did not exit");
            if (!Task.WaitAll(new[] { stdout, stderr }, 3000)) throw new TimeoutException("Driver log drain did not finish");
            process.Dispose();
        }
    }
}
