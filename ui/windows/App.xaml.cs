using Drift.Core;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;

namespace Drift;

public partial class App : Application
{
    private MainWindow? _window;

    public App()
    {
        InitializeComponent();
        // Never die silently: keep the reason next to the settings.
        UnhandledException += (_, e) =>
        {
            try
            {
                var dir = Shell.DataDir.Path;
                Directory.CreateDirectory(dir);
                File.AppendAllText(System.IO.Path.Combine(dir, "crash.log"), $"{DateTime.Now:O}{Environment.NewLine}{e.Exception}{Environment.NewLine}{Environment.NewLine}");
            }
            catch { }
        };
    }

    private static System.Threading.Mutex? _single;

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        // One Drift at a time (two would fight over Bluetooth): a second launch asks the running
        // one to show its window, then leaves. The mock (design) build has its own name.
        var argv0 = Environment.GetCommandLineArgs();
        var key = argv0.Contains("--mock") ? "Drift.Mock" : "Drift";
        _single = new System.Threading.Mutex(true, $"Local\\{key}.Single", out var first);
        var show = new EventWaitHandle(false, EventResetMode.AutoReset, $"Local\\{key}.Show");
        if (!first)
        {
            show.Set();
            Exit();
            return;
        }
        var dq = Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread();
        new Thread(() => { while (show.WaitOne()) dq.TryEnqueue(() => _window?.Summon()); }) { IsBackground = true, Name = "drift-single" }.Start();

        var dark = RequestedTheme == ApplicationTheme.Dark;
        Design.Design.Load(dark);
        ApplyTokens();

        // `--mock` runs the UI against a scripted core (design work without a phone).
        var argv = Environment.GetCommandLineArgs();
        ICore core = argv.Contains("--mock") ? new MockCore() : NativeCore.TryCreate() ?? (ICore)new MockCore();
        _window = new MainWindow(core);
        // `--tray` or the package's startup task (open at sign-in): start in the tray, window hidden
        var atSignIn = argv.Contains("--tray");
        try { atSignIn |= Microsoft.Windows.AppLifecycle.AppInstance.GetCurrent().GetActivatedEventArgs().Kind == Microsoft.Windows.AppLifecycle.ExtendedActivationKind.StartupTask; }
        catch { }
        if (!atSignIn) _window.Activate();
    }

    private void ApplyTokens()
    {
        // Bundled Geist / Geist Mono (Assets/Fonts), addressed by file so it works unpackaged.
        Resources["DriftText"] = new FontFamily(Stage.Fonts.Geist);
        Resources["DriftDisplay"] = new FontFamily(Stage.Fonts.Geist);
        Resources["DriftMono"] = new FontFamily(Stage.Fonts.GeistMono);
        void Set(string key, Windows.UI.Color c) => ((SolidColorBrush)Resources[key]).Color = c;
        Set("DriftBg", Design.Design.Bg);
        Set("DriftFg", Design.Design.Fg);
        Set("DriftGrey1", Design.Design.Grey1);
        Set("DriftGrey2", Design.Design.Grey2);
        // Lightweight styling for the built-in Button states: hover = grey2, press = inverted.
        Resources["ButtonBackgroundPointerOver"] = new SolidColorBrush(Design.Design.Grey2);
        Resources["ButtonForegroundPointerOver"] = new SolidColorBrush(Design.Design.Fg);
        Resources["ButtonBorderBrushPointerOver"] = new SolidColorBrush(Design.Design.Fg);
        Resources["ButtonBackgroundPressed"] = new SolidColorBrush(Design.Design.Fg);
        Resources["ButtonForegroundPressed"] = new SolidColorBrush(Design.Design.Bg);
        Resources["ButtonBorderBrushPressed"] = new SolidColorBrush(Design.Design.Fg);
    }
}
