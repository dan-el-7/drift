using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Velopack;

namespace Drift;

/// Our own entry point (DISABLE_XAML_GENERATED_MAIN): Velopack gets the first look, so the
/// installer's hooks run (and exit) before any window exists, then the WinUI app starts as usual.
public static class Program
{
    [STAThread]
    private static void Main(string[] args)
    {
        VelopackApp.Build()
            .OnBeforeUninstallFastCallback(_ => Shell.Cleanup.BeforeUninstall())
            .Run();

        WinRT.ComWrappersSupport.InitializeComWrappers();
        Application.Start(init =>
        {
            var context = new DispatcherQueueSynchronizationContext(DispatcherQueue.GetForCurrentThread());
            SynchronizationContext.SetSynchronizationContext(context);
            _ = new App();
        });
    }
}
