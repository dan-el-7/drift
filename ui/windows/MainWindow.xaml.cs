using System.Diagnostics;
using System.Numerics;
using System.Runtime.InteropServices;
using System.Text.Json;
using Drift.Core;
using Drift.Stage;
using Microsoft.Graphics.Canvas.UI.Xaml;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;
using Path = Microsoft.UI.Xaml.Shapes.Path;
using Windows.ApplicationModel.DataTransfer;
using Windows.Graphics;
using VirtualKey = Windows.System.VirtualKey;
using VirtualKeyModifiers = Windows.System.VirtualKeyModifiers;
using Windows.UI;
using D = Drift.Design.Design;
using CoreFile = Drift.Core.FileInfo;

namespace Drift;

/// The window: routes (home, devices, waiting, request, transfer, done, activity, settings), the
/// dock of running transfers, and the stage. Ported from design/prototype/motion-studies.html.
public sealed partial class MainWindow : Window
{
    private enum Route { Home, Devices, Waiting, Request, Transfer, Done, Activity, Settings }
    private enum TState { Connecting, Request, Waiting, Xfer, Done, Declined, Cancelled, Failed }

    private sealed class Transfer
    {
        public ulong Share;
        public bool Incoming;
        public string Peer = "Phone";
        public CoreFile[] Files = [];
        public string? Pin;
        public TState State;
        public long Done, Total;
        public double Bps;
        public string Medium = "bluetooth";
        public string? SavedPath;
        public int Texts;
        public long SetupMs;       // how long the fast link took to set up
        public bool? StayedOnline; // measured while on a direct/hosted link (null = not measured)
        public bool Drift, Trusted;  // the other side runs Drift / is a trusted Drift device
        public string? TextKind;   // url | text | address | phone
        public string? TextBody;   // what arrived
        public List<string>? SendPaths; // outgoing: what was sent (Try again after a failure)
        public long MovingSinceMs;       // when it started moving (to notice it's stuck on Bluetooth)
        public bool WarnedSlow;
        public long SlowSinceMs;         // on a fast link: since when it has been slow (0 = it isn't)
        public bool WarnedWeak;
        public bool IsLink => TextKind == "url" && TextBody is { } u && (u.StartsWith("http://", StringComparison.OrdinalIgnoreCase) || u.StartsWith("https://", StringComparison.OrdinalIgnoreCase));
        public long EndedMs;
        public double P => Total > 0 ? Math.Clamp((double)Done / Total, 0, 1) : 0;
        public long Size => Total > 0 ? Total : Files.Sum(f => f.Size);
        private string TextNoun => TextKind switch { "url" => "a link", "phone" => "a phone number", "address" => "an address", _ => "some text" };
        public string Title => Files.Length == 0 && Texts > 0 ? char.ToUpper(TextNoun[0]) + TextNoun[1..]
            : (Files.Length == 1 ? Files[0].Name : Files.Length > 1 ? $"{Files.Length} files" : "Files") + (Texts > 0 ? $" and {TextNoun}" : "");
        public bool TextOnly => Files.Length == 0 && Texts > 0;
        public bool Active => State is TState.Connecting or TState.Request or TState.Waiting or TState.Xfer;
    }

    private sealed class Settings
    {
        public string Name { get; set; } = Environment.MachineName;
        public string Look { get; set; } = "ink";
        public bool Details { get; set; }
        /// Visible to everyone nearby all the time (a tray app that can always receive).
        public bool AlwaysVisible { get; set; }
        /// Start Drift (in the tray) when you sign in to Windows.
        public bool OpenAtSignIn { get; set; }
        public string Folder { get; set; } = System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), "Downloads");
    }

    private readonly ICore _core;
    private readonly DispatcherQueue _dq;
    private readonly StageModel _m = new();
    private IStageRenderer _renderer = new InkRenderer();
    private Palette _pal;
    private readonly Stopwatch _clock = Stopwatch.StartNew();
    private double _last, _tick;
    private Settings _s = new();
    private bool _everyone = true;
    private double _visLeft = 600;

    private Route _route = Route.Home;
    private Transfer? _focus;
    private readonly List<Transfer> _transfers = new();
    private List<string> _sendPaths = new();
    private (string kind, string body)? _sendText; // a link or some text to send instead of files
    private string? _clipLink;                     // a link on the clipboard (Home offers to send it)

    [DllImport("user32.dll")] private static extern uint GetDpiForWindow(nint hwnd);

    public MainWindow(ICore core)
    {
        InitializeComponent();
        _core = core;
        _dq = DispatcherQueue.GetForCurrentThread();
        _pal = new Palette(D.Fg, D.Bg, D.Grey1, D.Grey2);
        Shell.DataDir.Migrate();
        LoadSettings();
        SetLook(_s.Look);
        SetupChrome();
        SetupShell();
        // updates: Drift only says there's one (never mid-transfer); the user decides
        _ = Shell.Updates.Run(v => _dq.TryEnqueue(() => { if (!_transfers.Any(t => t.Active)) OfferUpdate(v); }));

        Root.KeyboardAccelerators.Add(Accel(VirtualKey.Escape, VirtualKeyModifiers.None, () => Back()));
        // Ctrl+V sends what's copied, except while typing in a text box (that's a normal paste)
        var paste = new KeyboardAccelerator { Key = VirtualKey.V, Modifiers = VirtualKeyModifiers.Control };
        paste.Invoked += (sender, e) =>
        {
            if (Content.XamlRoot != null && FocusManager.GetFocusedElement(Content.XamlRoot) is TextBox) return;
            e.Handled = true;
            _ = PasteToSend();
        };
        Root.KeyboardAccelerators.Add(paste);
        Activated += (_, e) => { if (e.WindowActivationState != WindowActivationState.Deactivated) _ = CheckClipboard(); };
        if (core is MockCore mock)
        {
            // Dev: Ctrl+1 a phone sends a photo, Ctrl+2 an album.
            Root.KeyboardAccelerators.Add(Accel(VirtualKey.Number1, VirtualKeyModifiers.Control, () => mock.SimulateIncoming(1)));
            Root.KeyboardAccelerators.Add(Accel(VirtualKey.Number2, VirtualKeyModifiers.Control, () => mock.SimulateIncoming(2)));
            Root.KeyboardAccelerators.Add(Accel(VirtualKey.Number4, VirtualKeyModifiers.Control, () => mock.SimulateIncoming(3)));
            Root.KeyboardAccelerators.Add(Accel(VirtualKey.Number5, VirtualKeyModifiers.Control, () => mock.SimulateDrift()));
            // Ctrl+3: the notice card, as when Google's Quick Share is running
            Root.KeyboardAccelerators.Add(Accel(VirtualKey.Number3, VirtualKeyModifiers.Control, () => ShowNotice("Quit Google Quick Share", "Phones may reach it instead of Drift.", action: ("Quit it", QuitGoogleQuickShare))));
        }

        _core.Event += e => _dq.TryEnqueue(() => OnCore(e));
        Closed += (_, _) =>
        {
            Microsoft.UI.Xaml.Media.CompositionTarget.Rendering -= OnFrame;
            _visTimer?.Stop();
            _tray?.Dispose();
            _notices?.Dispose();
            _core.Dispose();
        };
        _core.Start(_s.Name, _s.Folder); // the name phones show
        _core.SetVisible(_everyone);     // the app opens visible for 10 minutes
        Microsoft.UI.Xaml.Media.CompositionTarget.Rendering += OnFrame;
        Go(Route.Home);
        if (Environment.GetCommandLineArgs().Contains("--demo")) _ = Demo();
        // Dev: `--notice` shows the notice card 1 s after start (for frame captures).
        if (Environment.GetCommandLineArgs().Contains("--notice")) Once(1000, () => ShowNotice("Quit Google Quick Share", "Phones may reach it instead of Drift.", action: ("Quit it", QuitGoogleQuickShare)));
        // Dev: `--link` makes the mock phone send a link 1 s after start.
        if (Environment.GetCommandLineArgs().Contains("--link") && _core is MockCore lm) Once(1000, () => lm.SimulateIncoming(3));
        // Dev: `--drift` has a mock Drift device send at 1 s and again at 9 s (trust flow).
        if (Environment.GetCommandLineArgs().Contains("--drift") && _core is MockCore dm) { Once(1000, dm.SimulateDrift); Once(9000, dm.SimulateDrift); }
        // Dev: `--send <file>...` goes straight to choosing a device (for testing without a picker).
        var args = Environment.GetCommandLineArgs();
        var si = Array.IndexOf(args, "--send");
        if (si >= 0) ChooseDevice(args.Skip(si + 1).TakeWhile(a => !a.StartsWith("--")).ToList());
    }

    private static KeyboardAccelerator Accel(VirtualKey k, VirtualKeyModifiers mods, Action a)
    {
        var acc = new KeyboardAccelerator { Key = k, Modifiers = mods };
        acc.Invoked += (_, e) => { e.Handled = true; a(); };
        return acc;
    }

    private void SetupChrome()
    {
        ExtendsContentIntoTitleBar = true;
        SetTitleBar(DragRegion);
        var hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
        var scale = GetDpiForWindow(hwnd) / 96.0;
        var size = new SizeInt32((int)(D.WindowWidth * scale), (int)(D.WindowHeight * scale));
        if (AppWindow.Presenter is OverlappedPresenter p)
        {
            p.IsResizable = false;
            p.IsMaximizable = false;
            p.SetBorderAndTitleBar(true, false);
        }
        // After the title bar is gone: the content area is exactly 380×520
        // (Resize would include the invisible borders, and resizing first would leave the old title bar's height).
        AppWindow.ResizeClient(size);
        // Windows can still hand the hidden caption's height to the content; correct from the
        // laid-out size so the design's 380×520 is exact.
        Root.SizeChanged += (_, e) =>
        {
            var dw = D.WindowWidth - e.NewSize.Width;
            var dh = D.WindowHeight - e.NewSize.Height;
            if (Math.Abs(dw) < 0.5 && Math.Abs(dh) < 0.5) return;
            // Resize the outer window by the measured difference (ResizeClient keeps re-adding the
            // removed caption's height, so it can't converge here).
            var cur = AppWindow.Size;
            AppWindow.Resize(new SizeInt32((int)Math.Round(cur.Width + dw * scale), (int)Math.Round(cur.Height + dh * scale)));
        };
        var area = DisplayArea.GetFromWindowId(AppWindow.Id, DisplayAreaFallback.Nearest).WorkArea;
        AppWindow.Move(new PointInt32(area.X + (area.Width - size.Width) / 2, area.Y + (area.Height - size.Height) / 2));
        AppWindow.Title = "Drift";
    }

    // =========================================================================== core events

    private Transfer? Find(ulong share) => _transfers.FirstOrDefault(t => t.Share == share);

    private void OnCore(CoreEvent e)
    {
        switch (e.T)
        {
            case "warning":
                ShowNotice(e.Text ?? "Something needs a look", e.Detail,
                    action: e.Code == "quick_share" ? ("Quit it", QuitGoogleQuickShare) : null);
                if (!InFront) _notices?.Failed(e.Text ?? "Drift needs a look");
                break;
            case "device_found" when e.Id != null && _route == Route.Devices:
                // Link details adds whether it runs Drift; otherwise just the name and kind
                AddDevice(e.Id, e.Name ?? e.Id, (e.Kind ?? "") + (_s.Details && e.Drift ? " · drift" : ""));
                break;
            case "device_lost" when e.Id != null:
                _m.RemoveNode(e.Id);
                break;
            case "incoming":
            {
                var t = new Transfer { Share = e.Share, Incoming = true, Peer = e.From ?? "A phone", Files = e.Files ?? [], Pin = e.Pin, State = TState.Request, Texts = e.Texts, TextKind = e.TextKind, Drift = e.Drift, Trusted = e.Trusted };
                _transfers.Insert(0, t);
                if (e.Auto)
                {
                    // a trusted Drift device: no prompt, straight to the transfer
                    t.State = TState.Xfer;
                    if (_route is Route.Home or Route.Done or Route.Activity) Go(Route.Transfer, t); else RenderDock();
                    break;
                }
                if (_route is Route.Home or Route.Done or Route.Activity) Go(Route.Request, t);
                else RenderDock(); // it waits in the dock, pulsing
                if (!InFront) _notices?.Incoming(t.Share, t.Peer, $"{t.Title}", t.Pin);
                break;
            }
            case "trusted":
                _trustedDevices = e.Devices ?? [];
                if (_route == Route.Settings) { _sheetSig = ""; RenderSheet(); }
                break;
            case "waiting":
            {
                // Our send got through to the phone: it now shows the same code.
                var t = _transfers.FirstOrDefault(t => !t.Incoming && t.Share == 0 && t.State == TState.Connecting)
                        ?? Find(e.Share);
                if (t == null) { t = new Transfer { Incoming = false }; _transfers.Insert(0, t); }
                t.Share = e.Share; t.Pin = e.Pin; t.State = TState.Waiting;
                if (e.Name != null) t.Peer = e.Name;
                if (e.Files is { Length: > 0 }) t.Files = e.Files;
                if (_focus == t && _route == Route.Waiting) Go(Route.Waiting, t);
                else RenderDock();
                break;
            }
            case "accepted" when Find(e.Share) is { } t:
                t.State = TState.Xfer;
                if (_focus == t && _route is Route.Waiting or Route.Request) Go(Route.Transfer, t);
                else RenderDock();
                break;
            case "medium" when Find(e.Share) is { } t:
                t.Medium = e.Medium ?? t.Medium;
                if (e.SetupMs > 0) t.SetupMs = e.SetupMs;
                break;
            case "progress" when Find(e.Share) is { } t:
                t.Done = e.Done; t.Total = e.Total; t.Bps = e.Bps;
                if (t.State is TState.Waiting or TState.Request or TState.Connecting) t.State = TState.Xfer;
                break;
            case "text" when Find(e.Share) is { } t:
                t.TextBody = e.Text;
                t.TextKind = e.TextKind ?? t.TextKind;
                break;
            case "saved" when Find(e.Share) is { } t:
                t.SavedPath = e.Path;
                break;
            case "done" when Find(e.Share) is { } t:
                switch (e.Outcome)
                {
                    case "completed": Finish(t, TState.Done, null); break;
                    case "declined": Finish(t, TState.Declined, t.Incoming ? null : $"{t.Peer} didn't accept"); break;
                    case "cancelled": Finish(t, TState.Cancelled, null); break;
                    case "stalled": Finish(t, TState.Failed, "The link stopped. Try again: Drift will use another one"); break;
                    default: Finish(t, TState.Failed, e.Error ?? "Something went wrong"); break;
                }
                break;
        }
    }

    // ----------------------------------------------------------------- Blitz (easter egg)
    private readonly List<long> _blitzTaps = new();
    private readonly HashSet<ulong> _blitzUnlocked = new();
    private long _blitzAt;
    /// The Wi-Fi network Blitz took us off, to reconnect when the transfer ends.
    private string? _blitzProfile;

    private void OnBlitzTap(object sender, Microsoft.UI.Xaml.Input.PointerRoutedEventArgs e)
    {
        if (_route != Route.Transfer || _focus is not { State: TState.Xfer } t || _blitzUnlocked.Contains(t.Share)) return;
        var now = _clock.ElapsedMilliseconds;
        _blitzTaps.RemoveAll(x => now - x > 3000);
        _blitzTaps.Add(now);
        if (_blitzTaps.Count < 7) return;
        _blitzTaps.Clear();
        _blitzUnlocked.Add(t.Share);
        _blitzAt = now;
        RenderFooter(false);
    }

    /// Free the radio of the side that splits its airtime. On a Wi-Fi Direct link that's this PC (its
    /// Wi-Fi network on one channel, the phone's group on another): drop our Wi-Fi for the transfer
    /// (no admin needed) and reconnect when it ends. On the phone's own hotspot the phone is the one.
    private void RunBlitz(Transfer t)
    {
        if (t.Medium == "same_hotspot") { ShowNotice("Blitz is on the phone's side here", "On the phone: turn Wi-Fi off for this transfer. Its hotspot then runs at full speed.", 7000); return; }
        if (!IsDirect(t.Medium)) { ShowNotice("Blitz needs a direct link", "This transfer runs over your network or Bluetooth.", 5000); return; }
        try
        {
            var o = Netsh("wlan show interfaces");
            var line = o.Split('\n').FirstOrDefault(l => l.TrimStart().StartsWith("Profile ") || l.TrimStart().StartsWith("Profile\t"));
            var profile = line?.Split(':', 2).ElementAtOrDefault(1)?.Trim();
            if (string.IsNullOrEmpty(profile)) { ShowNotice("Already at full speed", "This PC isn't on another Wi-Fi network.", 5000); return; }
            _blitzProfile = profile;
            Netsh("wlan disconnect");
            ShowNotice("Blitz: Wi-Fi off for this transfer", $"The direct link gets the whole radio. Back on {profile} when it's done.", 6000);
        }
        catch { ShowNotice("Couldn't turn Wi-Fi off", "Try it from Quick Settings instead.", 6000); }
    }

    private void EndBlitz()
    {
        if (_blitzProfile is not { } p) return;
        _blitzProfile = null;
        try { Netsh($"wlan connect name=\"{p}\""); } catch { }
    }

    private static string Netsh(string args)
    {
        using var proc = System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo("netsh", args)
        {
            UseShellExecute = false, CreateNoWindow = true, RedirectStandardOutput = true,
        })!;
        var o = proc.StandardOutput.ReadToEnd();
        proc.WaitForExit(5000);
        return o;
    }

    private void Finish(Transfer t, TState state, string? msg)
    {
        _notices?.Remove(t.Share);
        EndBlitz();
        if (!InFront)
        {
            if (state == TState.Done && t.Incoming && t.TextOnly && t.TextBody != null) _notices?.TextReceived(t.Share, $"{t.Title} from {t.Peer}", t.TextBody, t.IsLink);
            else if (state == TState.Done && t.Incoming) _notices?.Saved(t.Share, t.Title, t.SavedPath);
            else if (msg != null) _notices?.Failed(msg);
        }
        t.State = state;
        t.EndedMs = _clock.ElapsedMilliseconds;
        if (_focus == t && _route is Route.Transfer or Route.Waiting or Route.Request)
        {
            if (state == TState.Done) Go(Route.Done, t);
            else { Go(Route.Home); Toast(msg, Retry(t, state)); }
        }
        else { Toast(msg, Retry(t, state)); RenderDock(); }
    }

    // =========================================================================== routes

    private void Back()
    {
        if (_route == Route.Home) return;
        if (_route == Route.Devices) _core.Scan(false);
        Go(Route.Home);
    }

    private void Go(Route route, Transfer? focus = null)
    {
        _route = route;
        if (focus != null) _focus = focus;
        var t = _focus;

        BackButton.Visibility = route == Route.Home ? Visibility.Collapsed : Visibility.Visible;
        Mark.Visibility = route == Route.Home ? Visibility.Visible : Visibility.Collapsed;
        HomeIcons.Visibility = route == Route.Home ? Visibility.Visible : Visibility.Collapsed;
        Meta.Margin = new Thickness(12, 0, route == Route.Home ? 6 : 0, 0); // prototype: 6px gap to the icons, else flush; never touching the title
        Crumb.Visibility = route == Route.Home ? Visibility.Collapsed : Visibility.Visible;
        Crumb.Text = route switch
        {
            Route.Devices or Route.Waiting => "Send",
            Route.Request => "Receive",
            Route.Transfer => t?.Incoming == true ? "Receiving" : "Sending",
            Route.Activity => "Activity",
            Route.Settings => "Settings",
            _ => "",
        };

        var sheet = route is Route.Activity or Route.Settings;
        switch (route)
        {
            case Route.Home or Route.Activity or Route.Settings: _m.Pose(StageMode.Idle, dim: sheet); break;
            case Route.Devices: _m.Pose(StageMode.Devices); break;
            case Route.Waiting when t?.Pin == null: _m.Pose(StageMode.Devices); break; // connecting: no code yet
            case Route.Waiting or Route.Request: _m.Pose(StageMode.Pin, pin: t!.Pin, peer: t.Peer); break;
            case Route.Transfer: _m.Pose(StageMode.Ring, peer: t!.Peer, p: t.P); break;
            case Route.Done: _m.Pose(StageMode.Done); break;
        }
        if (route != Route.Devices && !(route == Route.Waiting && t?.Pin == null)) { Nodes.Children.Clear(); }

        SetInverted(false);
        _doneTimer?.Stop();
        if (route == Route.Done)
        {
            _invertTimer = Once(D.ReducedMotion ? 0 : 150, () => { if (_route == Route.Done) SetInverted(true); });
            _doneTimer = Once(6000, () => { if (_route == Route.Done) Go(Route.Home); });
        }

        ShowSheet(sheet);
        Headline.Visibility = sheet ? Visibility.Collapsed : Visibility.Visible;
        Actions.Visibility = sheet ? Visibility.Collapsed : Visibility.Visible;
        if (sheet) { _sheetSig = ""; RenderSheet(); }
        RenderFooter(fresh: true);
        RenderDock();
    }

    private DispatcherQueueTimer? _doneTimer, _invertTimer;
    // Pending one-shot timers are held here until they fire: a DispatcherQueueTimer nobody
    // references can be collected before it ticks, and its callback silently never runs.
    private readonly HashSet<DispatcherQueueTimer> _pending = new();
    private DispatcherQueueTimer Once(double ms, Action a)
    {
        var t = _dq.CreateTimer();
        t.Interval = TimeSpan.FromMilliseconds(Math.Max(1, ms));
        t.IsRepeating = false;
        t.Tick += (_, _) => { _pending.Remove(t); a(); };
        _pending.Add(t);
        t.Start();
        return t;
    }

    // =========================================================================== footer

    private void RenderFooter(bool fresh)
    {
        var t = _focus;
        string h = "", p = "";
        var acts = new List<Button>();
        switch (_route)
        {
            case Route.Home:
                h = _everyone ? $"Visible as {_s.Name}" : "Hidden";
                p = !_everyone ? "Only you can send from here · drop files to send"
                    : _s.AlwaysVisible ? "Everyone nearby can see you · drop files to send"
                    : $"Everyone can see you for {Clock(_visLeft)} · drop files to send";
                acts.Add(Pill("Send files", true, () => _ = PickFiles()));
                if (_clipLink is { } cl) acts.Add(Pill("Send link", false, () => ChooseDeviceText("url", cl)));
                if (!_everyone) acts.Add(Pill("Be visible", false, () => SetEveryone(true)));
                break;
            case Route.Devices:
                h = _sendText is { } st ? (st.kind == "url" ? "Send a link" : "Send some text")
                    : $"Send {(_sendPaths.Count == 1 ? System.IO.Path.GetFileName(_sendPaths[0]) : $"{_sendPaths.Count} files")}";
                p = _m.Nodes.Count > 0 ? "Choose a device" : "Looking for nearby devices…";
                acts.Add(Pill("Cancel", false, () => { _core.Scan(false); Go(Route.Home); }));
                if (_sendText == null) acts.Add(Pill("Add files", false, () => _ = PickFiles(add: true)));
                break;
            case Route.Waiting when t != null:
                h = t.Pin == null ? $"Connecting to {t.Peer}…" : $"Waiting for {t.Peer}";
                p = t.Pin == null ? "Hold on, this takes a moment" : "Check the code matches, then accept on the phone";
                acts.Add(Pill("Hide", false, Back));
                acts.Add(Pill("Cancel send", false, () => { if (t.Share != 0) _core.Cancel(t.Share); Finish(t, TState.Cancelled, null); }));
                break;
            case Route.Request when t != null:
                h = $"{t.Peer} wants to send";
                p = t.TextOnly ? t.Title : $"{t.Title} · {Mb(t.Size)}";
                acts.Add(Pill("Decline", false, () => { _core.Respond(t.Share, false); Finish(t, TState.Declined, null); }));
                // a Drift device: accept and trust it, so next time there's no prompt
                if (t.Drift && !t.Trusted)
                    acts.Add(Pill("Always accept", false, () => { _core.Respond(t.Share, true); _core.Trust(t.Share); t.Trusted = true; _notices?.Remove(t.Share); t.State = TState.Xfer; Go(Route.Transfer, t); }));
                acts.Add(Pill("Accept", true, () => { _core.Respond(t.Share, true); _notices?.Remove(t.Share); t.State = TState.Xfer; Go(Route.Transfer, t); }));
                break;
            case Route.Transfer when t != null:
                h = t.Title;
                p = XferLine(t);
                acts.Add(Pill("Hide", false, Back));
                acts.Add(Pill("Cancel", false, () => _core.Cancel(t.Share)));
                if (_blitzUnlocked.Contains(t.Share) && t.State == TState.Xfer) acts.Add(Pill("Blitz", true, () => RunBlitz(t)));
                break;
            case Route.Done when t is { Incoming: true, TextOnly: true }:
            {
                // a link or some text: show it, open it (http/https only), copy it
                h = t.TextKind == "url" ? "Link received" : t.TextKind == "phone" ? "Number received" : t.TextKind == "address" ? "Address received" : "Text received";
                p = (t.TextBody ?? "").ReplaceLineEndings(" ");
                var body = t.TextBody ?? "";
                if (t.IsLink) acts.Add(Pill("Open", true, () => Launch(body), inverted: true));
                acts.Add(Pill("Copy", !t.IsLink, () => { CopyText(body); Go(Route.Home); }, inverted: true));
                break;
            }
            case Route.Done when t != null:
                h = t.Incoming ? $"Saved to {System.IO.Path.GetFileName(_s.Folder.TrimEnd('\\'))}" : $"Sent to {t.Peer}";
                p = t.Title + (_s.Details && t.StayedOnline is { } stayed ? (stayed ? " · PC stayed online" : " · PC went offline") : "");
                if (t.Incoming)
                {
                    var saved = t.SavedPath;
                    acts.Add(Pill("Open", true, () => Launch(saved ?? _s.Folder), inverted: true));
                    acts.Add(Pill("Show in folder", false, () => Launch("explorer.exe", saved != null ? $"/select,\"{saved}\"" : $"\"{_s.Folder}\""), inverted: true));
                }
                else acts.Add(Pill("Done", true, () => Go(Route.Home), inverted: true));
                break;
            case Route.Activity:
            {
                var a = _transfers.Count(t => t.Active);
                p = _transfers.Count == 0 ? "Nothing yet" : $"{a} running · {_transfers.Count - a} finished";
                break;
            }
            case Route.Settings:
                if (fresh) _settingsPage = "main";
                p = $"Drift {Shell.Updates.Version ?? "dev"} · works with Quick Share";
                if (Shell.Updates.Available is { } upd) acts.Add(Pill($"Update to {upd}", true, () => _ = UpdateNow()));
                // the core's log, for a problem report (it restarts with Drift: this session only)
                acts.Add(Pill("Show log", false, () => Launch("explorer.exe", $"/select,\"{System.IO.Path.Combine(Shell.DataDir.Path, "core.log")}\"")));
                break;
        }
        Headline.Text = h;
        Subline.Text = p;
        // Rebuild the buttons only when they change (never under the pointer on a timer tick).
        var actSig = string.Join("|", acts.Select(b => b.Content as string)) + _route;
        if (fresh || actSig != _actSig)
        {
            _actSig = actSig;
            var root = Content?.XamlRoot; // null until the window is first shown
            var keyboard = root != null && Microsoft.UI.Xaml.Input.FocusManager.GetFocusedElement(root) is Control { FocusState: FocusState.Keyboard };
            Actions.Children.Clear();
            foreach (var b in acts) Actions.Children.Add(b);
            // Removing the focused button would push focus (and its rectangle) onto the title bar.
            // Keep focus in the footer, and only show a focus ring if the keyboard was in use.
            if (root != null && acts.Count > 0) acts[0].Focus(keyboard ? FocusState.Keyboard : FocusState.Pointer);
        }
        if (fresh) Rise(Footer);
    }

    private string _actSig = "";

    private string XferLine(Transfer t)
    {
        if (t.State != TState.Xfer) return "Waiting";
        var line = $"{Mb(t.Done)} of {Mb(t.Size)} · {Math.Floor(t.P * 100)}%";
        // Link details: whether this PC kept its internet goes here (the title bar has no room)
        if (_s.Details && t.StayedOnline is { } on) line += on ? " · PC online" : " · PC went offline";
        return line;
    }

    private static string Clock(double s) => $"{(int)s / 60}:{(int)s % 60:00}";
    private static string Mb(long b) => b >= 1_000_000_000 ? $"{b / 1e9:0.00} GB" : b >= 10_000_000 ? $"{b / 1e6:0} MB" : $"{Math.Max(0.1, b / 1e6):0.0} MB";

    private string LinkName(Transfer t) => t.Medium switch
    {
        "bluetooth" => "Bluetooth",
        "lan" => "Wi-Fi",
        "hotspot" or "direct" or "phone_hosted" => "Direct link",
        "same_hotspot" => "Hotspot",
        _ => t.Medium,
    };

    /// Link details (Settings): which link, how fast it came up, and, measured, whether the PC
    /// stayed online while it ran (the point of a direct link).
    private string LinkDetail(Transfer t, bool live = false)
    {
        // live, in the title bar beside the rate (~27 mono characters): short names only
        if (live) return t.Medium switch
        {
            "lan" => "Wi-Fi LAN",
            "direct" or "hotspot" => "Wi-Fi Direct",
            "phone_hosted" => "Phone Direct",
            "same_hotspot" => "Phone hotspot",
            _ => LinkName(t),
        };
        var name = t.Medium switch
        {
            "bluetooth" => "Bluetooth",
            "lan" => "Wi-Fi LAN",
            "hotspot" => "Hotspot",
            "direct" => "Wi-Fi Direct",
            "phone_hosted" => "Phone Direct",
            "same_hotspot" => "Phone hotspot",
            _ => t.Medium,
        };
        var parts = new List<string> { name };
        if (!live && t.SetupMs > 0 && t.Medium != "bluetooth") parts.Add($"set up {t.SetupMs / 1000.0:0.0} s");
        return string.Join(" · ", parts);
    }

    private static string Rate(double bps) => bps >= 10e6 ? $"{bps / 1e6:0} MB/s" : $"{bps / 1e6:0.0} MB/s";

    /// A transfer still on Bluetooth 12 s after it started moving (and big enough to matter): say why,
    /// once. From here we can't see the phone's Wi-Fi or hotspot, so name both likely causes.
    private void CheckStuckOnBluetooth()
    {
        foreach (var t in _transfers)
        {
            if (t.WarnedSlow || t.State != TState.Xfer || t.Size < 4_000_000 || t.Medium != "bluetooth") continue;
            var now = _clock.ElapsedMilliseconds;
            if (t.MovingSinceMs == 0) { t.MovingSinceMs = now; continue; }
            if (now - t.MovingSinceMs < 12_000) continue;
            t.WarnedSlow = true;
            ShowNotice("Stuck on Bluetooth", $"No faster link to {t.Peer}. Turn Wi-Fi on there (no network needed), and its hotspot off, or connect this PC to that hotspot.", 12000);
        }
        // a fast link that stays slow (under 8 MB/s for 10 s, on a big enough file): usually distance;
        // on the phone's hotspot, its Wi-Fi 6 mode can do it too (measured: ~3 MB/s vs 82 MB/s on Wi-Fi 5)
        foreach (var t in _transfers)
        {
            if (t.WarnedWeak || t.State != TState.Xfer || t.Size < 200_000_000 || t.Medium == "bluetooth") continue;
            var now = _clock.ElapsedMilliseconds;
            if (t.Bps >= 8_000_000 || t.Bps <= 0) { t.SlowSinceMs = 0; continue; }
            if (t.SlowSinceMs == 0) { t.SlowSinceMs = now; continue; }
            if (now - t.SlowSinceMs < 10_000) continue;
            t.WarnedWeak = true;
            var hotspot = t.Medium is "same_hotspot" or "hotspot";
            ShowNotice($"Slow link: you may be too far from {t.Peer}",
                "Move the devices closer." + (hotspot ? " (On the phone's hotspot, Wi-Fi 6 can slow it down too: set the hotspot to Wi-Fi 5.)" : ""), 12000);
        }
    }

    private static bool IsDirect(string medium) => medium is "hotspot" or "direct" or "phone_hosted";

    /// Once a second while a transfer runs on a direct link: does the PC still have internet?
    private void SampleOnline()
    {
        var running = _transfers.Where(t => t.State == TState.Xfer && IsDirect(t.Medium)).ToList();
        if (running.Count == 0) return;
        bool online;
        try
        {
            var p = Windows.Networking.Connectivity.NetworkInformation.GetInternetConnectionProfile();
            online = p != null && p.GetNetworkConnectivityLevel() == Windows.Networking.Connectivity.NetworkConnectivityLevel.InternetAccess;
        }
        catch { return; }
        foreach (var t in running) t.StayedOnline = (t.StayedOnline ?? true) && online;
    }

    private Button Pill(string text, bool solid, Action onClick, bool inverted = false)
    {
        var b = new Button
        {
            Content = text,
            Style = (Style)Application.Current.Resources[solid ? "PillSolid" : "Pill"],
        };
        if (inverted)
        {
            b.Foreground = Brush(solid ? D.Fg : D.Bg);
            b.Background = Brush(solid ? D.Bg : Microsoft.UI.Colors.Transparent);
            b.BorderBrush = Brush(D.Bg);
        }
        b.Click += (_, _) => onClick();
        return b;
    }

    private static SolidColorBrush Brush(Color c) => new(c);

    private void SetInverted(bool on)
    {
        var fg = Brush(on ? D.Bg : D.Fg);
        var dim = Brush(on ? D.Grey2 : D.Grey1);
        Headline.Foreground = fg;
        Subline.Foreground = dim;
        Mark.Foreground = fg;
        Crumb.Foreground = fg;
        Meta.Foreground = dim;
        MinButton.Foreground = CloseButton.Foreground = dim;
        if (BackButton.Content is Path path) path.Stroke = fg;
        if (on && _route == Route.Done) RenderFooterInverted();
    }

    private void RenderFooterInverted()
    {
        // Pills were built for the inverted surface in RenderFooter(Done); nothing else to do.
    }

    private static void CopyText(string text)
    {
        var dp = new DataPackage();
        dp.SetText(text);
        Clipboard.SetContent(dp);
        Clipboard.Flush(); // keeps it after Drift quits
    }

    private static void Launch(string file, string? args = null)
    {
        try { Process.Start(new ProcessStartInfo(file, args ?? "") { UseShellExecute = true }); }
        catch { }
    }

    /// One continuous motion: new footer content rises 7 px into place (the settle spring).
    private void Rise(UIElement el)
    {
        if (D.ReducedMotion) return;
        var v = Visual(el);
        Settle(v, "Translation", new Vector3(0, 7, 0), Vector3.Zero, 520);
        Settle(v, "Opacity", 0, 1, 520);
    }

    /// A dock chip arriving (prototype @keyframes chip): up 6px, from 96% scale, fading in, 420 ms.
    private void ChipIn(FrameworkElement el)
    {
        if (D.ReducedMotion) return;
        var v = Visual(el);
        void Center() => v.CenterPoint = new Vector3((float)el.ActualWidth / 2, (float)el.ActualHeight / 2, 0);
        el.SizeChanged += (_, _) => Center();
        Center();
        Settle(v, "Translation", new Vector3(0, 6, 0), Vector3.Zero, 420);
        Settle(v, "Scale", new Vector3(0.96f, 0.96f, 1), Vector3.One, 420);
        Settle(v, "Opacity", 0, 1, 420);
    }

    private bool _sheetShown;
    private DispatcherQueueTimer? _sheetTimer;
    /// Activity and Settings (prototype .panel): in, it fades over 250 ms and rises 14px on the settle
    /// curve; out, it drops back and fades over 200 ms. Moving between the two doesn't re-animate.
    private void ShowSheet(bool on)
    {
        if (on == _sheetShown) return;
        _sheetShown = on;
        _sheetTimer?.Stop();
        var v = Visual(Sheet);
        Sheet.IsHitTestVisible = on;
        if (on)
        {
            Sheet.Visibility = Visibility.Visible;
            if (D.ReducedMotion) { v.Opacity = 1; v.Properties.InsertVector3("Translation", Vector3.Zero); return; }
            Ease(v, "Opacity", 0, 1, 250);
            Settle(v, "Translation", new Vector3(0, 14, 0), Vector3.Zero, 520);
        }
        else
        {
            if (D.ReducedMotion) { Sheet.Visibility = Visibility.Collapsed; return; }
            v.StopAnimation("Translation");
            v.Properties.InsertVector3("Translation", new Vector3(0, 14, 0));
            v.StopAnimation("Opacity");
            Ease(v, "Opacity", v.Opacity, 0, 200);
            _sheetTimer = Once(200, () => { if (!_sheetShown) Sheet.Visibility = Visibility.Collapsed; });
        }
    }

    // ---- DOM-transition equivalents on the compositor

    private static Microsoft.UI.Composition.Visual Visual(UIElement el)
    {
        Microsoft.UI.Xaml.Hosting.ElementCompositionPreview.SetIsTranslationEnabled(el, true);
        return Microsoft.UI.Xaml.Hosting.ElementCompositionPreview.GetElementVisual(el);
    }

    /// The settle spring sampled into 41 points over 520 ms, last point 1: the prototype's
    /// `--settle-ease` linear() curve, so DOM-style transitions follow the same spring.
    private static float[]? _settleCurve;
    private static float[] SettleCurve()
    {
        if (_settleCurve != null) return _settleCurve;
        var s = new Sp(0, "settle").Go(1);
        var k = new float[41];
        for (var i = 0; i <= 40; i++) { k[i] = (float)s.X; s.Step(0.52 / 40, false); }
        k[40] = 1;
        return _settleCurve = k;
    }

    private static void Settle(Microsoft.UI.Composition.Visual v, string prop, float from, float to, double ms)
    {
        var c = v.Compositor;
        var a = c.CreateScalarKeyFrameAnimation();
        var lin = c.CreateLinearEasingFunction();
        var k = SettleCurve();
        for (var i = 0; i <= 40; i++) a.InsertKeyFrame(i / 40f, from + (to - from) * k[i], lin);
        a.Duration = TimeSpan.FromMilliseconds(ms);
        v.StartAnimation(prop, a);
    }

    private static void Settle(Microsoft.UI.Composition.Visual v, string prop, Vector3 from, Vector3 to, double ms)
    {
        var c = v.Compositor;
        var a = c.CreateVector3KeyFrameAnimation();
        var lin = c.CreateLinearEasingFunction();
        var k = SettleCurve();
        for (var i = 0; i <= 40; i++) a.InsertKeyFrame(i / 40f, from + (to - from) * k[i], lin);
        a.Duration = TimeSpan.FromMilliseconds(ms);
        v.StartAnimation(prop, a);
    }

    /// CSS `ease` (cubic-bezier(.25,.1,.25,1)), the default for the prototype's plain transitions.
    private static void Ease(Microsoft.UI.Composition.Visual v, string prop, float from, float to, double ms)
    {
        var c = v.Compositor;
        var a = c.CreateScalarKeyFrameAnimation();
        a.InsertKeyFrame(0, from);
        a.InsertKeyFrame(1, to, c.CreateCubicBezierEasingFunction(new Vector2(0.25f, 0.1f), new Vector2(0.25f, 1f)));
        a.Duration = TimeSpan.FromMilliseconds(ms);
        v.StartAnimation(prop, a);
    }

    // =========================================================================== notice card

    private Microsoft.UI.Xaml.Media.Animation.Storyboard? _noticeSb;
    private DispatcherQueueTimer? _noticeAuto;
    private DispatcherQueueTimer? _noticeHide;

    /// A lasting problem (Google's Quick Share running, firewall, Bluetooth): one card under the bar
    /// until dismissed. The "i" draws itself: ring, then stem, then the dot drops in and breathes.
    private Action? _noticeDo;
    private void ShowNotice(string title, string? detail, int autoHideMs = 0, (string label, Action run)? action = null)
    {
        _noticeDo = action?.run;
        NoticeActionText.Text = action?.label ?? "";
        NoticeAction.Visibility = action == null ? Visibility.Collapsed : Visibility.Visible;
        _noticeAuto?.Stop();
        if (autoHideMs > 0) _noticeAuto = Once(autoHideMs, HideNotice);
        NoticeTitle.Text = title;
        NoticeDetail.Text = detail ?? "";
        NoticeDetail.Visibility = string.IsNullOrEmpty(detail) ? Visibility.Collapsed : Visibility.Visible;
        _noticeHide?.Stop();
        NoticeCard.Visibility = Visibility.Visible;
        NoticeCard.IsHitTestVisible = true;
        var card = Visual(NoticeCard);
        var dot = Visual(NoticeDot);
        dot.CenterPoint = new Vector3(1.25f, 1.25f, 0);
        if (D.ReducedMotion)
        {
            card.Opacity = 1; card.Properties.InsertVector3("Translation", Vector3.Zero); card.Scale = Vector3.One;
            NoticeRing.StrokeDashOffset = 0; NoticeStem.StrokeDashOffset = 0;
            dot.Scale = Vector3.One; dot.Properties.InsertVector3("Translation", Vector3.Zero);
            return;
        }
        card.CenterPoint = new Vector3((float)NoticeCard.ActualWidth / 2, 0, 0);
        Ease(card, "Opacity", 0, 1, 250);
        Settle(card, "Translation", new Vector3(0, -8, 0), Vector3.Zero, 520);
        Settle(card, "Scale", new Vector3(0.98f, 0.98f, 1), Vector3.One, 520);

        // ring and stem: dash offsets drawn down to zero
        _noticeSb?.Stop();
        NoticeRing.StrokeDashOffset = 42; NoticeStem.StrokeDashOffset = 3.3;
        var sb = new Microsoft.UI.Xaml.Media.Animation.Storyboard();
        void Draw(DependencyObject target, string prop, double from, double ms, double delay)
        {
            var a = new Microsoft.UI.Xaml.Media.Animation.DoubleAnimation
            {
                From = from, To = 0, Duration = TimeSpan.FromMilliseconds(ms), BeginTime = TimeSpan.FromMilliseconds(delay),
                EnableDependentAnimation = true,
                EasingFunction = new Microsoft.UI.Xaml.Media.Animation.CubicEase { EasingMode = Microsoft.UI.Xaml.Media.Animation.EasingMode.EaseOut },
            };
            Microsoft.UI.Xaml.Media.Animation.Storyboard.SetTarget(a, target);
            Microsoft.UI.Xaml.Media.Animation.Storyboard.SetTargetProperty(a, prop);
            sb.Children.Add(a);
        }
        Draw(NoticeRing, "StrokeDashOffset", 42, 620, 60);
        Draw(NoticeStem, "StrokeDashOffset", 3.3, 300, 420);
        sb.Begin();
        _noticeSb = sb;

        // the dot: drops in with a little overshoot, then breathes
        var c = dot.Compositor;
        dot.Scale = Vector3.Zero;
        var drop = c.CreateVector3KeyFrameAnimation();
        var overshoot = c.CreateCubicBezierEasingFunction(new Vector2(0.3f, 1.6f), new Vector2(0.5f, 1f));
        drop.InsertKeyFrame(0, Vector3.Zero);
        drop.InsertKeyFrame(1, Vector3.One, overshoot);
        drop.DelayTime = TimeSpan.FromMilliseconds(560);
        drop.Duration = TimeSpan.FromMilliseconds(560);
        drop.DelayBehavior = Microsoft.UI.Composition.AnimationDelayBehavior.SetInitialValueBeforeDelay;
        dot.StartAnimation("Scale", drop);
        var fall = c.CreateVector3KeyFrameAnimation();
        fall.InsertKeyFrame(0, new Vector3(0, -6, 0));
        fall.InsertKeyFrame(1, Vector3.Zero, overshoot);
        fall.DelayTime = TimeSpan.FromMilliseconds(560);
        fall.Duration = TimeSpan.FromMilliseconds(560);
        fall.DelayBehavior = Microsoft.UI.Composition.AnimationDelayBehavior.SetInitialValueBeforeDelay;
        dot.StartAnimation("Translation", fall);
        Once(1200, () =>
        {
            if (NoticeCard.Visibility != Visibility.Visible) return;
            var b = c.CreateVector3KeyFrameAnimation();
            var io = c.CreateCubicBezierEasingFunction(new Vector2(0.42f, 0), new Vector2(0.58f, 1));
            b.InsertKeyFrame(0, Vector3.One);
            b.InsertKeyFrame(0.5f, new Vector3(1.35f, 1.35f, 1), io);
            b.InsertKeyFrame(1, Vector3.One, io);
            b.Duration = TimeSpan.FromMilliseconds(2400);
            b.IterationBehavior = Microsoft.UI.Composition.AnimationIterationBehavior.Forever;
            dot.StartAnimation("Scale", b);
        });
    }

    private void HideNotice()
    {
        if (NoticeCard.Visibility != Visibility.Visible) return;
        NoticeCard.IsHitTestVisible = false;
        if (D.ReducedMotion) { NoticeCard.Visibility = Visibility.Collapsed; return; }
        var card = Visual(NoticeCard);
        Ease(card, "Opacity", card.Opacity, 0, 200);
        card.StopAnimation("Translation");
        Settle(card, "Translation", Vector3.Zero, new Vector3(0, -6, 0), 300);
        _noticeHide = Once(200, () => { NoticeCard.Visibility = Visibility.Collapsed; Visual(NoticeDot).StopAnimation("Scale"); });
    }

    private void OnNoticeDismiss(object sender, RoutedEventArgs e) { _noticeAuto?.Stop(); HideNotice(); }
    private void OnNoticeAction(object sender, RoutedEventArgs e) { var a = _noticeDo; _noticeAuto?.Stop(); HideNotice(); a?.Invoke(); }

    /// F11: Google's Quick Share holds the same Bluetooth service. Quit it (your click), then
    /// re-register Drift's service so phones reach Drift.
    private void QuitGoogleQuickShare()
    {
        var any = false;
        foreach (var p in Process.GetProcessesByName("nearby_share"))
        {
            try { p.Kill(entireProcessTree: true); any = true; } catch { }
        }
        if (!any) return;
        if (_everyone) { _core.SetVisible(false); Once(800, () => { if (_everyone) _core.SetVisible(true); }); }
    }

    // =========================================================================== toast

    private DispatcherQueueTimer? _toastTimer;
    /// A send that failed: "Try again" opens the device picker with the same files (or text), so
    /// nobody has to find them again. The device is picked again too: it may have moved or renamed.
    private (string, Action)? Retry(Transfer t, TState state)
    {
        if (state != TState.Failed || t.Incoming) return null;
        if (t.SendPaths is { Count: > 0 } paths)
        {
            var still = paths.Where(File.Exists).ToList();
            return still.Count == 0 ? null : ("Try again", () => { HideNotice(); ChooseDevice(still); });
        }
        if (t.TextKind is { } k && t.TextBody is { } b) return ("Try again", () => { HideNotice(); ChooseDeviceText(k, b); });
        return null;
    }

    /// One-off failures use the notice card too, and leave on their own after a few seconds.
    /// "Title. Detail" or "Title: detail" splits into the card's two lines.
    private void Toast(string? msg, (string label, Action run)? action = null)
    {
        if (string.IsNullOrEmpty(msg)) return;
        var cut = msg.IndexOfAny(['.', ':']);
        var (title, detail) = cut > 0 && cut < msg.Length - 1
            ? (msg[..cut].Trim(), msg[(cut + 1)..].Trim().TrimEnd('.') is var d && d.Length > 0 ? char.ToUpper(d[0]) + d[1..] + "." : null)
            : (msg.TrimEnd('.'), null);
        // with Try again it stays until used or dismissed
        ShowNotice(title, detail, autoHideMs: action == null ? 5000 : 0, action: action);
    }

    // =========================================================================== dock

    private string _dockSig = "";
    private readonly Dictionary<Transfer, (Grid ring, TextBlock num)> _dockLive = new();

    private void RenderDock()
    {
        var now = _clock.ElapsedMilliseconds;
        var onScreen = _route is Route.Transfer or Route.Waiting or Route.Request or Route.Done ? _focus : null;
        var show = _route is Route.Home or Route.Devices or Route.Transfer or Route.Waiting or Route.Request;
        // Requests first (they need you), then running, then just finished.
        var list = _transfers.Where(t => t != onScreen && (t.Active || (t.State == TState.Done && now - t.EndedMs < 6000)))
            .OrderBy(t => t.State switch { TState.Request => 0, TState.Xfer => 1, TState.Waiting or TState.Connecting => 2, _ => 3 }).ToList();
        if (!show) list.Clear();
        // The dock is one line, 344 px wide: two chips at most, sized so both fit; the rest fold
        // into "+N", which opens Activity.
        var shown = list.Count <= 2 ? list : list.Take(1).ToList();
        var extra = list.Count - shown.Count;
        var textMax = shown.Count == 2 ? 92 : extra > 0 ? 196 : 250;
        var sig = string.Join("|", shown.Select(t => $"{t.GetHashCode()}:{t.State}")) + $"+{extra}";
        if (sig == _dockSig)
        {
            foreach (var (t, (ring, num)) in _dockLive)
            {
                MiniRing(ring, t.P);
                num.Text = $"{Math.Floor(t.P * 100)}%";
            }
            return;
        }
        _dockSig = sig;
        _dockLive.Clear();
        Dock.Children.Clear();
        foreach (var t in shown)
        {
            var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 7, VerticalAlignment = VerticalAlignment.Center };
            string txt;
            TextBlock? num = null;
            if (t.State is TState.Request or TState.Waiting or TState.Connecting)
            {
                row.Children.Add(Pulse());
                txt = t.State == TState.Request ? $"{t.Peer} wants to send" : $"Waiting for {t.Peer}";
            }
            else if (t.State == TState.Xfer)
            {
                var ring = new Grid { Width = 14, Height = 14 };
                MiniRing(ring, t.P);
                row.Children.Add(ring);
                txt = t.Title;
                num = new TextBlock { FontFamily = (FontFamily)Application.Current.Resources["DriftMono"], FontSize = 10.5, Foreground = Brush(D.Grey1), VerticalAlignment = VerticalAlignment.Center, Text = $"{Math.Floor(t.P * 100)}%" };
                _dockLive[t] = (ring, num);
            }
            else
            {
                row.Children.Add(DoneIcon());
                txt = t.Incoming ? $"Saved {t.Title}" : $"Sent to {t.Peer}";
            }
            row.Children.Add(new TextBlock { Text = txt, FontFamily = (FontFamily)Application.Current.Resources["DriftText"], FontSize = 11.5, FontWeight = Microsoft.UI.Text.FontWeights.Medium, Foreground = Brush(D.Fg), MaxWidth = textMax, TextTrimming = TextTrimming.CharacterEllipsis, VerticalAlignment = VerticalAlignment.Center });
            if (num != null) row.Children.Add(num);
            var chip = Chip(row);
            var target = t;
            chip.Click += (_, _) =>
            {
                if (_route == Route.Devices) _core.Scan(false);
                Go(target.State switch { TState.Request => Route.Request, TState.Waiting or TState.Connecting => Route.Waiting, TState.Xfer => Route.Transfer, _ => Route.Activity }, target);
            };
            Dock.Children.Add(chip);
            ChipIn(chip);
        }
        if (extra > 0)
        {
            var more = Chip(new TextBlock { Text = $"+{extra} more", FontSize = 11.5, FontWeight = Microsoft.UI.Text.FontWeights.Medium, Foreground = Brush(D.Fg), VerticalAlignment = VerticalAlignment.Center, FontFamily = (FontFamily)Application.Current.Resources["DriftText"] });
            more.Click += (_, _) => Go(Route.Activity);
            Dock.Children.Add(more);
            ChipIn(more);
        }
    }

    private static Button Chip(UIElement content) => new()
    {
        Style = Bare,
        Content = content,
        Height = 28,
        Padding = new Thickness(8, 0, 11, 0),
        CornerRadius = new CornerRadius(14),
        BorderThickness = new Thickness(1),
        BorderBrush = Brush(D.Grey2),
        Background = Brush(D.Bg),
        VerticalContentAlignment = VerticalAlignment.Center,
    };

    private static Style Bare => (Style)Application.Current.Resources["Bare"];

    /// The self-eating ring at 14 px: a XAML ellipse strokes inside its bounds, so a ring from
    /// radius Ro inward to Ri is just an ellipse of diameter 2·Ro with stroke Ro − Ri.
    private static void MiniRing(Grid g, double p)
    {
        const double s = 14.0 / 16;
        var ro = (6.5 + (2.4 - 6.5) * Math.Pow(p, 1.6)) * s;
        var ri = 5.5 * (1 - p) * s;
        if (g.Children.Count == 0)
        {
            g.Children.Add(new Ellipse { Width = 13 * s, Height = 13 * s, Stroke = Brush(D.Grey2), StrokeThickness = 1 });
            g.Children.Add(new Ellipse { Fill = null, Stroke = Brush(D.Fg) });
        }
        var e = (Ellipse)g.Children[1];
        e.Width = e.Height = 2 * ro;
        e.StrokeThickness = Math.Max(1, ro - ri);
    }

    private static UIElement Pulse()
    {
        var dot = new Ellipse { Width = 7, Height = 7, Fill = Brush(D.Fg), VerticalAlignment = VerticalAlignment.Center };
        if (!D.ReducedMotion)
        {
            var v = Microsoft.UI.Xaml.Hosting.ElementCompositionPreview.GetElementVisual(dot);
            var c = v.Compositor;
            v.CenterPoint = new Vector3(3.5f, 3.5f, 0);
            var io = c.CreateCubicBezierEasingFunction(new Vector2(0.42f, 0), new Vector2(0.58f, 1));
            var o = c.CreateScalarKeyFrameAnimation();
            o.InsertKeyFrame(0f, 1f);
            o.InsertKeyFrame(0.5f, 0.5f, io);
            o.InsertKeyFrame(1f, 1f, io);
            o.Duration = TimeSpan.FromMilliseconds(1200);
            o.IterationBehavior = Microsoft.UI.Composition.AnimationIterationBehavior.Forever;
            v.StartAnimation("Opacity", o);
            var sc = c.CreateVector3KeyFrameAnimation();
            sc.InsertKeyFrame(0f, Vector3.One);
            sc.InsertKeyFrame(0.5f, new Vector3(0.55f, 0.55f, 1), io);
            sc.InsertKeyFrame(1f, Vector3.One, io);
            sc.Duration = TimeSpan.FromMilliseconds(1200);
            sc.IterationBehavior = Microsoft.UI.Composition.AnimationIterationBehavior.Forever;
            v.StartAnimation("Scale", sc);
        }
        return dot;
    }

    private static UIElement DoneIcon()
    {
        var g = new Grid { Width = 14, Height = 14, VerticalAlignment = VerticalAlignment.Center };
        g.Children.Add(new Ellipse { Fill = Brush(D.Fg) });
        g.Children.Add(new Path
        {
            Data = (Geometry)Microsoft.UI.Xaml.Markup.XamlBindingHelper.ConvertValue(typeof(Geometry), "M4.4,7.2 L6.1,8.8 L9.6,5.1"),
            Stroke = Brush(D.Bg), StrokeThickness = 1.4, StrokeStartLineCap = PenLineCap.Round, StrokeEndLineCap = PenLineCap.Round,
        });
        return g;
    }

    // =========================================================================== sheets

    private string _sheetSig = "";
    /// Settings shows its list, or one of its cards ("trusted").
    private string _settingsPage = "main";
    private readonly Dictionary<Transfer, TextBlock> _activityStatus = new();

    private void RenderSheet()
    {
        if (_route == Route.Activity)
        {
            // the sheet's name leads the signature: an empty Activity must never look "unchanged"
            // (it used to leave Settings on screen when nothing had been transferred yet)
            var sig = "activity|" + string.Join("|", _transfers.Select(t => $"{t.GetHashCode()}:{t.State}"));
            if (sig == _sheetSig)
            {
                foreach (var (t, tb) in _activityStatus) tb.Text = Status(t);
                return;
            }
            _sheetSig = sig;
            _activityStatus.Clear();
            SheetBody.Children.Clear();
            if (_transfers.Count == 0)
            {
                SheetBody.Children.Add(new TextBlock { Text = "Nothing yet. Things you send and receive show up here.", Foreground = Brush(D.Grey1), FontSize = 13, Margin = new Thickness(8, 20, 8, 0), TextWrapping = TextWrapping.Wrap, FontFamily = (FontFamily)Application.Current.Resources["DriftText"] });
                return;
            }
            foreach (var t in _transfers)
            {
                var status = new TextBlock { Text = Status(t), FontFamily = (FontFamily)Application.Current.Resources["DriftMono"], FontSize = 10.5, Foreground = Brush(D.Grey1), VerticalAlignment = VerticalAlignment.Center, CharacterSpacing = 40 };
                _activityStatus[t] = status;
                var row = Row(t.State == TState.Done ? DoneIcon() : new Ellipse { Width = 13, Height = 13, Stroke = Brush(D.Grey1), StrokeThickness = 1 },
                    t.Title, $"{(t.Incoming ? "From" : "To")} {t.Peer} · {Mb(t.Size)}", status);
                var target = t;
                row.Click += (_, _) =>
                {
                    if (target.State == TState.Xfer) Go(Route.Transfer, target);
                    else if (target.State == TState.Request) Go(Route.Request, target);
                    else if (target.State is TState.Waiting or TState.Connecting) Go(Route.Waiting, target);
                    else if (target.SavedPath != null) Launch(target.SavedPath);
                };
                SheetBody.Children.Add(row);
            }
        }
        else if (_route == Route.Settings && _settingsPage == "trusted")
        {
            // Trusted devices open as their own card, so the settings list never grows with them.
            SheetBody.Children.Clear();
            var back = new Button
            {
                Style = Bare, Content = "\u2039  Trusted devices", FontSize = 13, Padding = new Thickness(8, 6, 10, 6), CornerRadius = new CornerRadius(12),
                Foreground = Brush(D.Fg), FontFamily = (FontFamily)Application.Current.Resources["DriftText"], Margin = new Thickness(0, 0, 0, 8),
            };
            back.Click += (_, _) => { _settingsPage = "main"; RenderSheet(); };
            SheetBody.Children.Add(back);
            var list = TrustedList();
            list.Margin = new Thickness(8, 0, 8, 0);
            SheetBody.Children.Add(list);
        }
        else if (_route == Route.Settings)
        {
            SheetBody.Children.Clear();
            var name = new TextBox { Text = _s.Name, MaxLength = 24, FontFamily = (FontFamily)Application.Current.Resources["DriftText"], FontSize = 13, Background = Brush(Microsoft.UI.Colors.Transparent), BorderThickness = new Thickness(0, 0, 0, 1), BorderBrush = Brush(D.Grey2), Foreground = Brush(D.Fg), Padding = new Thickness(0, 4, 0, 4) };
            void Commit() { var v = name.Text.Trim(); if (v.Length > 0 && v != _s.Name) { _s.Name = v; SaveSettings(); _core.Configure(_s.Name, _s.Folder); RenderSheet(); RenderFooter(false); } }
            name.LostFocus += (_, _) => Commit();
            name.KeyDown += (_, e) => { if (e.Key == VirtualKey.Enter) { e.Handled = true; Commit(); } };
            SheetBody.Children.Add(Setting("Name", name, $"Phones see \"{_s.Name}\"."));
            SheetBody.Children.Add(Setting("Visibility", Seg([("10 min", _everyone && !_s.AlwaysVisible), ("Always", _everyone && _s.AlwaysVisible), ("Hidden", !_everyone)], i =>
            {
                _s.AlwaysVisible = i == 1;
                SaveSettings();
                SetEveryone(i != 2);
            }), _s.AlwaysVisible ? "Phones nearby can always find this PC." : null));
            var folder = Pill("Change…", false, () => _ = PickFolder());
            SheetBody.Children.Add(Setting("Save to", folder, _s.Folder));
            SheetBody.Children.Add(Setting("Look", Seg([("Ink", _s.Look == "ink"), ("Monolith", _s.Look == "mono"), ("Field", _s.Look == "field")], i => { SetLook(i switch { 1 => "mono", 2 => "field", _ => "ink" }); SaveSettings(); RenderSheet(); }), null));
            SheetBody.Children.Add(Setting("Link details", Toggle(_s.Details, v => { _s.Details = v; SaveSettings(); }), "Adds the link type and whether your Wi-Fi stayed on."));
            var trusted = new Button
            {
                Style = Bare, FontSize = 13, Padding = new Thickness(10, 4, 8, 5), CornerRadius = new CornerRadius(12),
                Foreground = Brush(D.Grey1), FontFamily = (FontFamily)Application.Current.Resources["DriftText"],
                Content = (_trustedDevices.Length == 0 ? "None" : _trustedDevices.Length == 1 ? _trustedDevices[0].Name : $"{_trustedDevices.Length} devices") + "  \u203a",
            };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(trusted, "Trusted devices");
            trusted.Click += (_, _) => { _settingsPage = "trusted"; RenderSheet(); };
            SheetBody.Children.Add(Setting("Trusted devices", trusted, null));
            SheetBody.Children.Add(Setting("Open at sign-in", Toggle(_s.OpenAtSignIn, v => { _s.OpenAtSignIn = v; SaveSettings(); SetOpenAtSignIn(v); }), "Starts quietly in the tray, ready to receive."));
            Button? reset = null;
            reset = Pill(_btResetting ? "Restarting…" : "Restart Bluetooth", false, () => _ = ResetBluetooth(reset!));
            reset.IsEnabled = !_btResetting;
            SheetBody.Children.Add(Setting("Bluetooth", reset, "If a phone sees this PC but can't connect. Your other Bluetooth devices will reconnect."));
            Button? wfd = null;
            wfd = Pill(_wfdResetting ? "Resetting…" : "Reset pairing", false, () => _ = ResetWifiDirect(wfd!));
            wfd.IsEnabled = !_wfdResetting;
            SheetBody.Children.Add(Setting("Wi-Fi Direct", wfd, "If a phone's direct link stops connecting. The phone asks once more next time."));
        }
    }

    private bool _wfdResetting;

    /// Forget every Wi-Fi Direct pairing (and the device ids Drift remembered, p2p-devices.txt), so the
    /// next direct link pairs from scratch. Windows' lookup of Wi-Fi Direct devices takes ~10 s.
    private async Task ResetWifiDirect(Button b)
    {
        if (_wfdResetting) return;
        _wfdResetting = true;
        b.Content = "Resetting…"; b.IsEnabled = false;
        var n = 0;
        try
        {
            var sel = Windows.Devices.WiFiDirect.WiFiDirectDevice.GetDeviceSelector(Windows.Devices.WiFiDirect.WiFiDirectDeviceSelectorType.AssociationEndpoint)
                + " AND System.Devices.Aep.IsPaired:=System.StructuredQueryType.Boolean#True";
            foreach (var d in await Windows.Devices.Enumeration.DeviceInformation.FindAllAsync(sel))
            {
                var r = await d.Pairing.UnpairAsync();
                if (r.Status == Windows.Devices.Enumeration.DeviceUnpairingResultStatus.Unpaired) n++;
            }
            var ids = System.IO.Path.Combine(Shell.DataDir.Path, "p2p-devices.txt");
            if (System.IO.File.Exists(ids)) System.IO.File.Delete(ids);
            ShowNotice(n == 0 ? "Wi-Fi Direct was already clear" : $"Forgot {n} Wi-Fi Direct pairing{(n == 1 ? "" : "s")}", "The phone asks once to connect next time.", 5000);
        }
        catch { ShowNotice("Couldn't reset Wi-Fi Direct", "Try again in a moment.", 6000); }
        finally
        {
            _wfdResetting = false;
            if (_route == Route.Settings) RenderSheet();
        }
    }

    private string Status(Transfer t) => t.State switch
    {
        TState.Request => "WAITING FOR YOU",
        TState.Waiting or TState.Connecting => "WAITING",
        TState.Xfer => $"{Math.Floor(t.P * 100)}%",
        TState.Done => t.Incoming ? "SAVED" : "SENT",
        TState.Declined => "DECLINED",
        TState.Cancelled => "CANCELLED",
        _ => "FAILED",
    };

    private Button Row(UIElement icon, string title, string sub, UIElement right)
    {
        var g = new Grid { ColumnSpacing = 12 };
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(22) });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        if (icon is FrameworkElement fe) fe.VerticalAlignment = VerticalAlignment.Center;
        g.Children.Add(icon);
        var texts = new StackPanel { Spacing = 1 };
        texts.Children.Add(new TextBlock { Text = title, FontSize = 13, FontWeight = Microsoft.UI.Text.FontWeights.Medium, Foreground = Brush(D.Fg), TextTrimming = TextTrimming.CharacterEllipsis, FontFamily = (FontFamily)Application.Current.Resources["DriftText"] });
        texts.Children.Add(new TextBlock { Text = sub, FontSize = 11.5, Foreground = Brush(D.Grey1), TextTrimming = TextTrimming.CharacterEllipsis, FontFamily = (FontFamily)Application.Current.Resources["DriftText"] });
        Grid.SetColumn(texts, 1);
        g.Children.Add(texts);
        Grid.SetColumn((FrameworkElement)right, 2);
        g.Children.Add(right);
        return new Button
        {
            Style = Bare,
            Content = g, HorizontalAlignment = HorizontalAlignment.Stretch, HorizontalContentAlignment = HorizontalAlignment.Stretch,
            Background = Brush(Microsoft.UI.Colors.Transparent), BorderThickness = new Thickness(0, 0, 0, 1), BorderBrush = Brush(D.Grey2),
            CornerRadius = new CornerRadius(0), Padding = new Thickness(8, 11, 8, 11),
        };
    }

    private TrustedDevice[] _trustedDevices = [];

    private FrameworkElement TrustedList()
    {
        var list = new StackPanel { Spacing = 4 };
        if (_trustedDevices.Length == 0)
        {
            list.Children.Add(new TextBlock { Text = "None yet", FontSize = 12.5, Foreground = Brush(D.Grey1), FontFamily = (FontFamily)Application.Current.Resources["DriftText"] });
            return list;
        }
        foreach (var d in _trustedDevices)
        {
            var row = new Grid { ColumnSpacing = 8 };
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            var name = new StackPanel { VerticalAlignment = VerticalAlignment.Center };
            name.Children.Add(new TextBlock { Text = d.Name, FontSize = 13, Foreground = Brush(D.Fg), FontFamily = (FontFamily)Application.Current.Resources["DriftText"] });
            name.Children.Add(new TextBlock { Text = d.Id.ToUpperInvariant(), FontSize = 10.5, CharacterSpacing = 40, Foreground = Brush(D.Grey1), FontFamily = (FontFamily)Application.Current.Resources["DriftMono"] });
            row.Children.Add(name);
            var id = d.Id;
            var remove = new Button
            {
                Style = Bare, Content = "Remove", FontSize = 12, Padding = new Thickness(10, 4, 10, 5), CornerRadius = new CornerRadius(12),
                Foreground = Brush(D.Grey1), FontFamily = (FontFamily)Application.Current.Resources["DriftText"], VerticalAlignment = VerticalAlignment.Center,
            };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(remove, $"Remove {d.Name}");
            remove.Click += (_, _) => _core.Untrust(id);
            Grid.SetColumn(remove, 1);
            row.Children.Add(remove);
            list.Children.Add(row);
        }
        return list;
    }

    private FrameworkElement Setting(string label, FrameworkElement control, string? note)
    {
        var p = new StackPanel { Spacing = 8, Padding = new Thickness(8, 14, 8, 14), BorderThickness = new Thickness(0, 0, 0, 1), BorderBrush = Brush(D.Grey2) };
        p.Children.Add(new TextBlock { Text = label, FontSize = 13, FontWeight = Microsoft.UI.Text.FontWeights.Medium, Foreground = Brush(D.Fg), FontFamily = (FontFamily)Application.Current.Resources["DriftText"] });
        p.Children.Add(control);
        if (note != null) p.Children.Add(new TextBlock { Text = note, FontSize = 12, Foreground = Brush(D.Grey1), TextWrapping = TextWrapping.Wrap, FontFamily = (FontFamily)Application.Current.Resources["DriftText"] });
        return p;
    }

    private FrameworkElement Seg((string label, bool on)[] items, Action<int> pick)
    {
        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2 };
        for (var i = 0; i < items.Length; i++)
        {
            var idx = i;
            var b = new Button
            {
                Style = Bare, Height = 26, VerticalContentAlignment = VerticalAlignment.Center,
                Content = items[i].label, FontSize = 12, FontWeight = Microsoft.UI.Text.FontWeights.Medium, Padding = new Thickness(12, 0, 12, 1), CornerRadius = new CornerRadius(13), BorderThickness = new Thickness(0),
                Background = Brush(items[i].on ? D.Fg : Microsoft.UI.Colors.Transparent), Foreground = Brush(items[i].on ? D.Bg : D.Grey1),
                FontFamily = (FontFamily)Application.Current.Resources["DriftText"],
            };
            b.Click += (_, _) => pick(idx);
            row.Children.Add(b);
        }
        return new Border { Child = row, BorderThickness = new Thickness(1), BorderBrush = Brush(D.Grey2), CornerRadius = new CornerRadius(16), Padding = new Thickness(2), HorizontalAlignment = HorizontalAlignment.Left };
    }

    private FrameworkElement Toggle(bool on, Action<bool> set)
    {
        var thumb = new Border { Width = 14, Height = 14, CornerRadius = new CornerRadius(7), HorizontalAlignment = HorizontalAlignment.Left, Margin = new Thickness(3, 0, 0, 0) };
        var track = new Border { Width = 34, Height = 20, CornerRadius = new CornerRadius(10), Child = thumb };
        var first = true;
        void Paint()
        {
            track.Background = Brush(on ? D.Fg : D.Grey2);
            thumb.Background = Brush(on ? D.Bg : D.Grey1);
            var v = Visual(thumb);
            var to = new Vector3(on ? 14 : 0, 0, 0);
            if (first || D.ReducedMotion) v.Properties.InsertVector3("Translation", to);
            else Settle(v, "Translation", new Vector3(on ? 0 : 14, 0, 0), to, 250);
            if (first && !D.ReducedMotion)
            {
                track.BackgroundTransition = new BrushTransition { Duration = TimeSpan.FromMilliseconds(200) };
                thumb.BackgroundTransition = new BrushTransition { Duration = TimeSpan.FromMilliseconds(200) };
            }
            first = false;
        }
        Paint();
        var b = new Button { Style = Bare, Content = track, CornerRadius = new CornerRadius(10), HorizontalAlignment = HorizontalAlignment.Left };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(b, "Link details");
        b.Click += (_, _) => { on = !on; Paint(); set(on); };
        return b;
    }

    // =========================================================================== looks + settings

    private void SetLook(string look)
    {
        _s.Look = look;
        _renderer = look switch { "mono" => new MonolithRenderer(), "field" => new FieldRenderer(), _ => new InkRenderer() };
    }

    // =========================================================================== tray, notifications

    private Shell.Tray? _tray;
    private Shell.Notices? _notices;
    private DispatcherQueueTimer? _visTimer;
    private bool _quitting;
    private nint _hwnd;

    [System.Runtime.InteropServices.DllImport("user32.dll")] private static extern nint GetForegroundWindow();
    [System.Runtime.InteropServices.DllImport("user32.dll")] private static extern bool SetForegroundWindow(nint hWnd);

    private void SetupShell()
    {
        _hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
        var icon = System.IO.Path.Combine(AppContext.BaseDirectory, "Assets", "Drift.ico");
        if (File.Exists(icon))
        {
            AppWindow.SetIcon(icon);
            _tray = new Shell.Tray(_hwnd, icon, ShowFromTray, Quit);
        }
        _notices = new Shell.Notices();
        _notices.Invoked += a => _dq.TryEnqueue(() => OnNotice(a));
        // × and Alt+F4 tuck Drift into the tray (it keeps receiving); Quit is in the tray menu.
        AppWindow.Closing += (_, e) => { if (!_quitting) { e.Cancel = true; AppWindow.Hide(); } };
        if (_s.AlwaysVisible) _everyone = true;
        _visTimer = _dq.CreateTimer();
        _visTimer.Interval = TimeSpan.FromSeconds(1);
        _visTimer.Tick += (_, _) =>
        {
            if (_everyone && !_s.AlwaysVisible) { _visLeft -= 1; if (_visLeft <= 0) SetEveryone(false); }
            _tray?.SetTip(_everyone ? (_s.AlwaysVisible ? "Drift · visible" : $"Drift · visible {Clock(_visLeft)}") : "Drift · hidden");
            SampleOnline();
        };
        _visTimer.Start();
    }

    /// In front and visible: no need for a Windows notification.
    private bool InFront => AppWindow.IsVisible && GetForegroundWindow() == _hwnd;

    private void ShowFromTray()
    {
        AppWindow.Show();
        Activate();
        SetForegroundWindow(_hwnd);
    }

    private void OfferUpdate(string v) =>
        ShowNotice($"Drift {v} is available", "Update now, or keep this version for now.", action: ("Update", () => _ = UpdateNow()));

    private async Task UpdateNow()
    {
        if (_transfers.Any(t => t.Active)) { ShowNotice("Finish the transfer first", "Then update from Settings.", 5000); return; }
        ShowNotice("Updating Drift", "Downloading. Drift restarts when it's done.");
        if (!await Shell.Updates.Install()) ShowNotice("Couldn't update", "Check your connection and try again.", 6000);
    }

    private void Quit()
    {
        _quitting = true;
        Close();
    }

    private void OnNotice(IDictionary<string, string> a)
    {
        a.TryGetValue("act", out var act);
        var t = a.TryGetValue("share", out var sid) && ulong.TryParse(sid, out var id) ? Find(id) : null;
        switch (act)
        {
            case "accept" when t is { State: TState.Request }:
                _core.Respond(t.Share, true);
                t.State = TState.Xfer;
                if (_route == Route.Request && _focus == t) Go(Route.Transfer, t); else RenderDock();
                break;
            case "decline" when t is { State: TState.Request }:
                _core.Respond(t.Share, false);
                Finish(t, TState.Declined, null);
                break;
            case "openlink" when t is { IsLink: true }:
                Launch(t.TextBody!);
                break;
            case "copy" when t?.TextBody != null:
                CopyText(t.TextBody);
                break;
            case "openfile" when t != null:
                Launch(t.SavedPath ?? _s.Folder);
                break;
            case "folder" when t != null:
                Launch("explorer.exe", t.SavedPath != null ? $"/select,\"{t.SavedPath}\"" : $"\"{_s.Folder}\"");
                break;
            default: // the notification itself: bring Drift up on that share
                ShowFromTray();
                if (t is { State: TState.Request }) Go(Route.Request, t);
                else if (t is { State: TState.Xfer }) Go(Route.Transfer, t);
                break;
        }
    }

    /// Running from the MSIX? (Package.Current throws when unpackaged.)
    public static bool IsPackaged
    {
        get { try { return Windows.ApplicationModel.Package.Current != null; } catch { return false; } }
    }

    /// Open at sign-in. Packaged: the manifest's startup task (Task Manager > Startup apps lists it).
    /// Dev builds: an HKCU Run entry (this user only, no admin). Either way Drift starts in the tray.
    private async void SetOpenAtSignIn(bool on)
    {
        if (IsPackaged)
        {
            try
            {
                var task = await Windows.ApplicationModel.StartupTask.GetAsync("DriftAtSignIn");
                if (!on) { task.Disable(); return; }
                var state = await task.RequestEnableAsync();
                if (state is Windows.ApplicationModel.StartupTaskState.DisabledByUser or Windows.ApplicationModel.StartupTaskState.DisabledByPolicy)
                {
                    _s.OpenAtSignIn = false; SaveSettings(); RenderSheet();
                    ShowNotice("Windows has Drift off at sign-in", "Turn it on in Task Manager > Startup apps.", 7000);
                }
            }
            catch { }
            return;
        }
        try
        {
            using var run = Microsoft.Win32.Registry.CurrentUser.CreateSubKey(@"Software\Microsoft\Windows\CurrentVersion\Run");
            if (on) run.SetValue("Drift", $"\"{Environment.ProcessPath}\" --tray");
            else run.DeleteValue("Drift", throwOnMissingValue: false);
        }
        catch { }
    }

    /// Bring the window up (from the tray, or when Drift is launched a second time).
    public void Summon() => ShowFromTray();

    private bool _btResetting;
    /// F8: Bluetooth stops taking connections (phone gets page timeouts) while our advert is still
    /// visible. Turning the radio off and on clears it. Drift's own Bluetooth service is stopped
    /// first and brought back after, so it re-registers on the fresh radio.
    private async Task ResetBluetooth(Button b)
    {
        if (_btResetting) return;
        _btResetting = true;
        b.Content = "Restarting…"; b.IsEnabled = false;
        var wasVisible = _everyone;
        try
        {
            if (await Windows.Devices.Radios.Radio.RequestAccessAsync() != Windows.Devices.Radios.RadioAccessStatus.Allowed)
            { ShowNotice("Windows didn't allow a Bluetooth restart", "Turn Bluetooth off and on in Quick Settings instead.", 6000); return; }
            // The adapter's own radio: Radio.GetRadiosAsync() comes back empty for an unpackaged app.
            var adapter = await Windows.Devices.Bluetooth.BluetoothAdapter.GetDefaultAsync();
            var radio = adapter != null ? await adapter.GetRadioAsync() : null;
            radio ??= (await Windows.Devices.Radios.Radio.GetRadiosAsync()).FirstOrDefault(r => r.Kind == Windows.Devices.Radios.RadioKind.Bluetooth);
            if (radio == null) { ShowNotice("Couldn't reach Bluetooth", "Turn it off and on in Quick Settings instead.", 6000); return; }
            if (wasVisible) _core.SetVisible(false);
            await radio.SetStateAsync(Windows.Devices.Radios.RadioState.Off);
            await Task.Delay(1500);
            await radio.SetStateAsync(Windows.Devices.Radios.RadioState.On);
            await Task.Delay(2000); // let the stack come back before we advertise again
        }
        catch { ShowNotice("Couldn't restart Bluetooth", "Turn it off and on in Quick Settings instead.", 6000); }
        finally
        {
            if (wasVisible && _everyone) _core.SetVisible(true);
            _btResetting = false;
            if (_route == Route.Settings) { _sheetSig = ""; RenderSheet(); }
        }
    }

    private void SetEveryone(bool on)
    {
        _everyone = on;
        _visLeft = 600;
        _core.SetVisible(on);
        if (_route == Route.Settings) RenderSheet();
        if (_route == Route.Home) RenderFooter(false);
    }

    private static string SettingsPath => System.IO.Path.Combine(Shell.DataDir.Path, "settings.json");

    private void LoadSettings()
    {
        try { if (File.Exists(SettingsPath)) _s = JsonSerializer.Deserialize<Settings>(File.ReadAllText(SettingsPath)) ?? new(); }
        catch { _s = new(); }
    }

    private void SaveSettings()
    {
        try
        {
            Directory.CreateDirectory(System.IO.Path.GetDirectoryName(SettingsPath)!);
            File.WriteAllText(SettingsPath, JsonSerializer.Serialize(_s));
        }
        catch { }
    }

    // =========================================================================== sending

    /// add: while choosing a device, the picked files join the ones already chosen.
    private async Task PickFiles(bool add = false)
    {
        var picker = new Windows.Storage.Pickers.FileOpenPicker();
        picker.FileTypeFilter.Add("*");
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        var files = await picker.PickMultipleFilesAsync();
        if (files.Count > 0) ChooseDevice(files.Select(f => f.Path).ToList(), add);
    }

    private async Task PickFolder()
    {
        var picker = new Windows.Storage.Pickers.FolderPicker();
        picker.FileTypeFilter.Add("*");
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        var f = await picker.PickSingleFolderAsync();
        if (f == null) return;
        _s.Folder = f.Path;
        SaveSettings();
        _core.Configure(_s.Name, _s.Folder);
        RenderSheet();
    }

    private void ChooseDevice(List<string> paths, bool add = false)
    {
        if (paths.Count == 0) return;
        // more files while choosing a device: add them (dropped or picked), don't start over
        if (add && _route == Route.Devices && _sendText == null)
        {
            // the devices found so far stay; only the heading changes ("Send 3 files")
            _sendPaths = _sendPaths.Concat(paths).Distinct(StringComparer.OrdinalIgnoreCase).ToList();
            RenderFooter(false);
            return;
        }
        _sendPaths = paths;
        _sendText = null;
        StartChoosing();
    }

    /// Send a link or some text: same device picker as files.
    private void ChooseDeviceText(string kind, string body)
    {
        body = body.Trim();
        if (body.Length == 0) return;
        if (body.Length > 20_000) body = body[..20_000];
        _sendPaths = new();
        _sendText = (kind, body);
        StartChoosing();
    }

    private static bool IsWebLink(string s) =>
        Uri.TryCreate(s.Trim(), UriKind.Absolute, out var u) && (u.Scheme == Uri.UriSchemeHttp || u.Scheme == Uri.UriSchemeHttps) && !s.Trim().Contains(' ');

    /// Ctrl+V: copied files, a link, or some text, straight to the device picker.
    private async Task PasteToSend()
    {
        try
        {
            var c = Clipboard.GetContent();
            if (c.Contains(StandardDataFormats.StorageItems))
            {
                var items = await c.GetStorageItemsAsync();
                ChooseDevice(items.OfType<Windows.Storage.StorageFile>().Select(f => f.Path).Where(p => !string.IsNullOrEmpty(p)).ToList());
            }
            else if (c.Contains(StandardDataFormats.Text))
            {
                var text = await c.GetTextAsync();
                ChooseDeviceText(IsWebLink(text) ? "url" : "text", text);
            }
        }
        catch { }
    }

    /// When Drift comes to the front: is there a link on the clipboard to offer?
    private async Task CheckClipboard()
    {
        string? link = null;
        try
        {
            var c = Clipboard.GetContent();
            if (c.Contains(StandardDataFormats.Text)) { var t = (await c.GetTextAsync()).Trim(); if (t.Length < 2048 && IsWebLink(t)) link = t; }
        }
        catch { }
        if (link == _clipLink) return;
        _clipLink = link;
        if (_route == Route.Home) RenderFooter(false);
    }

    private void StartChoosing()
    {
        _m.ClearNodes();
        Nodes.Children.Clear();
        Go(Route.Devices);
        _core.Scan(true);
    }

    private void AddDevice(string id, string name, string kind)
    {
        if (_m.Nodes.Any(n => n.Id == id && n.Tag != null)) return;
        var n = _m.AddNode(id, name, kind);
        var label = new StackPanel { Spacing = 2, HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Bottom };
        label.Children.Add(new TextBlock { Text = name, FontSize = 12, FontWeight = Microsoft.UI.Text.FontWeights.Medium, Foreground = Brush(D.Fg), HorizontalAlignment = HorizontalAlignment.Center, FontFamily = (FontFamily)Application.Current.Resources["DriftText"] });
        if (kind.Length > 0)
            label.Children.Add(new TextBlock { Text = kind.ToUpperInvariant(), FontSize = 10.5, CharacterSpacing = 40, Foreground = Brush(D.Grey1), HorizontalAlignment = HorizontalAlignment.Center, FontFamily = (FontFamily)Application.Current.Resources["DriftMono"] });
        // The button covers the drop (the click target) and keeps the label clear below it.
        var b = new Button
        {
            Style = Bare, Content = label, Width = 120, Height = 78, Padding = new Thickness(4, 0, 4, 4), VerticalContentAlignment = VerticalAlignment.Bottom,
            CornerRadius = new CornerRadius(12), Opacity = 0,
        };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(b, $"Send to {name}");
        Canvas.SetLeft(b, n.Tx - 60);
        Canvas.SetTop(b, n.Ty - 22);
        b.Click += (_, _) => SendTo(id, name);
        Nodes.Children.Add(b);
        n.Tag = b;
        RenderFooter(false);
    }

    private void SendTo(string id, string name)
    {
        _core.Scan(false);
        var files = _sendPaths.Select(p => new CoreFile(System.IO.Path.GetFileName(p), File.Exists(p) ? new System.IO.FileInfo(p).Length : 0)).ToArray();
        var t = new Transfer { Incoming = false, Peer = name, Files = files, State = TState.Connecting, SendPaths = _sendPaths.ToList() };
        if (_sendText is { } st) { t.Texts = 1; t.TextKind = st.kind; t.TextBody = st.body; }
        _transfers.Insert(0, t);
        _core.Send(id, _sendPaths, _sendText is { } s2 ? [s2] : null);
        Go(Route.Waiting, t);
    }

    private void OnDragOver(object sender, DragEventArgs e)
    {
        if (!e.DataView.Contains(StandardDataFormats.StorageItems) && !e.DataView.Contains(StandardDataFormats.WebLink) && !e.DataView.Contains(StandardDataFormats.Text)) return;
        e.AcceptedOperation = DataPackageOperation.Copy;
        if (e.DragUIOverride != null) { e.DragUIOverride.Caption = "Send"; e.DragUIOverride.IsGlyphVisible = false; }
        DragRing.Visibility = Visibility.Visible;
    }

    private void OnDragLeave(object sender, DragEventArgs e) => DragRing.Visibility = Visibility.Collapsed;

    private async void OnDrop(object sender, DragEventArgs e)
    {
        DragRing.Visibility = Visibility.Collapsed;
        if (_route == Route.Devices && !e.DataView.Contains(StandardDataFormats.StorageItems)) _core.Scan(false);
        if (e.DataView.Contains(StandardDataFormats.WebLink)) { ChooseDeviceText("url", (await e.DataView.GetWebLinkAsync()).ToString()); return; }
        if (!e.DataView.Contains(StandardDataFormats.StorageItems))
        {
            if (e.DataView.Contains(StandardDataFormats.Text)) { var tx = await e.DataView.GetTextAsync(); ChooseDeviceText(IsWebLink(tx) ? "url" : "text", tx); }
            return;
        }
        var items = await e.DataView.GetStorageItemsAsync();
        var paths = items.OfType<Windows.Storage.StorageFile>().Select(f => f.Path).Where(p => !string.IsNullOrEmpty(p)).ToList();
        ChooseDevice(paths, add: true);
    }

    // =========================================================================== frame loop

    /// Dev: `--pin-at <ms>` holds the code reveal at that moment (for frame-by-frame comparison).
    private static readonly double PinAt = Environment.GetCommandLineArgs() is var a && Array.IndexOf(a, "--pin-at") is var i && i >= 0 && i + 1 < a.Length && double.TryParse(a[i + 1], System.Globalization.CultureInfo.InvariantCulture, out var v) ? v : -1;
    private bool _reducedCache;
    private double _reducedCheck;

    private void OnFrame(object? sender, object e)
    {
        var now = _clock.Elapsed.TotalSeconds;
        var dt = Math.Min(0.05, now - _last);
        _last = now;
        if (now - _reducedCheck > 1) { _reducedCheck = now; _reducedCache = D.ReducedMotion; }
        _m.Reduced = _reducedCache;
        _m.W = StageCanvas.ActualWidth > 0 ? StageCanvas.ActualWidth : 380;
        _m.H = StageCanvas.ActualHeight > 0 ? StageCanvas.ActualHeight : 520;
        _m.Cx = _m.W / 2;

        var f = _focus;
        if (_route == Route.Transfer && f != null)
        {
            _m.P.Go(f.P, "settle");
            _m.Rate = f.Bps / 1e6;
        }
        _m.Step(dt);
        if (PinAt >= 0 && _m.Pin != null) _m.PinStart = _m.T - PinAt / 1000; // dev: freeze the code reveal
        StageCanvas.Opacity = Sp.Clamp(_m.Dim.X); // the stage recedes behind Activity and Settings
        StageCanvas.Invalidate();

        // crisp overlays shared by every look
        var showPct = _s.Look != "field" && _m.Ring.X > 0.01;
        var p = Sp.Clamp(_m.P.X);
        Pct.Opacity = showPct ? Sp.Clamp(1 - (p - 0.3) * 3.2) * Sp.Clamp(_m.Ring.X) : 0;
        Pct.Text = $"{Math.Floor(p * 100)}%";
        Pct.Measure(new Windows.Foundation.Size(double.PositiveInfinity, double.PositiveInfinity));
        Canvas.SetLeft(Pct, _m.Cx - Pct.DesiredSize.Width / 2);
        Canvas.SetTop(Pct, _m.Cy - Pct.DesiredSize.Height / 2);
        Peer.Opacity = 0; // drawn with the stage (OnDraw), so it moves with its drop in the same frame
        foreach (var n in _m.Nodes)
            if (n.Tag is Button b)
            {
                // the label arrives once the drop has pinched off and settled
                b.Opacity = Sp.Clamp((n.K.X - 0.8) / 0.2) * Sp.Clamp(n.A.X);
                b.IsHitTestVisible = n.A.To > 0;
            }

        // title-bar meta: visibility at home, the live link while a transfer is on screen
        // Blitz: the hit area rides the ring; the mark draws on, fills, swells once, then stays
        BlitzHit.Visibility = _route == Route.Transfer && f is { State: TState.Xfer } ? Visibility.Visible : Visibility.Collapsed;
        Canvas.SetLeft(BlitzHit, _m.Cx - 90);
        Canvas.SetTop(BlitzHit, _m.Cy - 90);
        var markOn = _route == Route.Transfer && f != null && _blitzUnlocked.Contains(f.Share);
        BlitzMark.Visibility = markOn ? Visibility.Visible : Visibility.Collapsed;
        if (markOn)
        {
            BlitzMark.Margin = new Thickness(8 + Mark.ActualWidth + 6, 0, 0, 1);
            var ms = (_clock.ElapsedMilliseconds - _blitzAt) * (_m.Reduced ? 100 : 1);
            var draw = Sp.Clamp(ms / 420.0);
            var fill = Sp.Clamp((ms - 420) / 140.0);
            var flash = ms < 560 ? 0 : ms < 670 ? (ms - 560) / 110.0 : Sp.Clamp(1 - (ms - 670) / 420.0);
            BlitzMark.StrokeDashArray = new DoubleCollection { 40, 40 };
            BlitzMark.StrokeDashOffset = 40 * (1 - draw);
            BlitzMark.Fill = fill > 0 ? new SolidColorBrush(Windows.UI.Color.FromArgb((byte)(255 * fill), D.Fg.R, D.Fg.G, D.Fg.B)) : null;
            BlitzScale.ScaleX = BlitzScale.ScaleY = 1 + 0.3 * flash;
        }
        Meta.Text = _route switch
        {
            Route.Transfer when f != null => $"{(_s.Details ? LinkDetail(f, live: true) : LinkName(f))} · {Rate(f.Bps)}".ToUpperInvariant(),
            Route.Done when f != null && _s.Details && f.Medium != "bluetooth" => LinkDetail(f).ToUpperInvariant(),
            Route.Home => _everyone ? (_s.AlwaysVisible ? "VISIBLE" : $"VISIBLE {Clock(_visLeft)}") : "HIDDEN",
            Route.Waiting or Route.Request when f?.Pin != null => $"CODE {f.Pin}",
            _ => "",
        };

        _tick += dt;
        if (_tick > 0.125)
        {
            _tick = 0;
            if (_route == Route.Transfer && f != null) Subline.Text = XferLine(f);
            CheckStuckOnBluetooth();
            if (_route == Route.Home) RenderFooter(false);
            if (_route == Route.Activity) { RenderSheet(); RenderFooter(false); }
            RenderDock();
        }
    }

    private void OnDraw(CanvasControl sender, CanvasDrawEventArgs args)
    {
        var dt = 1 / 60.0; // particles only; springs step in OnFrame
        _renderer.Draw(args.DrawingSession, sender, _m, _pal, dt);
        DrawPeerName(args.DrawingSession);
    }

    private Microsoft.Graphics.Canvas.Text.CanvasTextFormat? _peerFormat;
    /// The phone's name, right of its drop and centred on it. Drawn in the stage's own pass (sub-pixel,
    /// same frame as the drop), so it rides the Ink drop's bob instead of stepping beside it.
    private void DrawPeerName(Microsoft.Graphics.Canvas.CanvasDrawingSession ds)
    {
        var a = Sp.Clamp(_m.Peer.X) * (_m.Mode == StageMode.Done ? 0 : 1);
        if (a < 0.01 || string.IsNullOrEmpty(_m.PeerName)) return;
        _peerFormat ??= new Microsoft.Graphics.Canvas.Text.CanvasTextFormat
        {
            FontFamily = Stage.Fonts.Win2DGeist, FontSize = 12, FontWeight = Microsoft.UI.Text.FontWeights.Medium,
            VerticalAlignment = Microsoft.Graphics.Canvas.Text.CanvasVerticalAlignment.Center,
            WordWrapping = Microsoft.Graphics.Canvas.Text.CanvasWordWrapping.NoWrap,
        };
        var bob = _s.Look == "ink" && !_m.Reduced ? 2.2 * Math.Sin(_m.T * 1.3 + 0.4) : 0;
        var y = _m.PeerY.X + bob;
        var col = Windows.UI.Color.FromArgb((byte)(255 * a), D.Grey1.R, D.Grey1.G, D.Grey1.B);
        try { ds.DrawText(_m.PeerName, new Windows.Foundation.Rect(_m.Cx + 22, y - 10, 200, 20), col, _peerFormat); }
        catch (ArgumentException) { Stage.Fonts.Win2DFallback(); _peerFormat = null; }
    }

    // =========================================================================== chrome

    private void OnBack(object sender, RoutedEventArgs e) => Back();
    private void OnActivity(object sender, RoutedEventArgs e) => Go(Route.Activity);
    private void OnSettings(object sender, RoutedEventArgs e) => Go(Route.Settings);
    private void OnMinimize(object sender, RoutedEventArgs e) { if (AppWindow.Presenter is OverlappedPresenter p) p.Minimize(); }
    private void OnClose(object sender, RoutedEventArgs e) => AppWindow.Hide(); // to the tray; Quit is in its menu

    // =========================================================================== demo (--demo): a scripted tour for screenshots

    private async Task Demo()
    {
        if (_core is not MockCore mock) return;
        async Task Wait(double s) => await Task.Delay(TimeSpan.FromSeconds(s));
        await Wait(2.5); mock.SimulateIncoming(1);                         // request
        await Wait(3.0); _core.Respond(_focus!.Share, true); _focus.State = TState.Xfer; Go(Route.Transfer, _focus); // ring
        await Wait(2.4); Back();                                            // hide -> dock
        await Wait(1.0); ChooseDevice([System.IO.Path.Combine(AppContext.BaseDirectory, "Drift.dll"), System.IO.Path.Combine(AppContext.BaseDirectory, "Drift.exe")]);
        await Wait(4.0); var first = _m.Nodes.FirstOrDefault(); if (first != null) SendTo(first.Id, first.Name);
        await Wait(1.5); mock.SimulateIncoming(1);                         // a second request while sending
        await Wait(0.4); mock.SimulateIncoming(2);                         // and a third: the dock has to fold
        await Wait(4.0); Go(Route.Activity);
        await Wait(2.0); Go(Route.Settings);
        await Wait(2.0); Go(Route.Home);
    }
}
