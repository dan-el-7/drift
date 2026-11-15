using System.Diagnostics;

namespace Drift.Shell;

/// What uninstalling Drift removes besides its files: the sign-in entry and the firewall rule.
/// Runs from Velopack's uninstall hook (it must finish quickly; the rule removal shows one UAC prompt,
/// only if the rule exists).
public static class Cleanup
{
    private const string RuleName = "Drift (Wi-Fi Direct)";

    public static void BeforeUninstall()
    {
        try
        {
            using var run = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Run", writable: true);
            run?.DeleteValue("Drift", throwOnMissingValue: false);
        }
        catch { }
        try
        {
            // One elevated call for both: the firewall rule, and the Wi-Fi Direct ports Drift told to ignore
            // group default routes (routes.rs, route-guard.txt) go back to Windows' default.
            var guard = System.IO.Path.Combine(DataDir.Path, "route-guard.txt");
            var ports = System.IO.File.Exists(guard)
                ? System.IO.File.ReadAllLines(guard).Where(l => int.TryParse(l.Trim(), out _)).Select(l => l.Trim()).ToArray()
                : [];
            var rule = RuleExists();
            if (!rule && ports.Length == 0) return;
            var script = (rule ? $"netsh advfirewall firewall delete rule name='{RuleName}'; " : "")
                + (ports.Length > 0 ? $"foreach ($i in @({string.Join(",", ports)})) {{ Set-NetIPInterface -InterfaceIndex $i -AddressFamily IPv4 -IgnoreDefaultRoutes Disabled -ErrorAction SilentlyContinue }}" : "");
            var p = Process.Start(new ProcessStartInfo("powershell", $"-NoProfile -Command \"{script}\"")
            {
                UseShellExecute = true, Verb = "runas", WindowStyle = ProcessWindowStyle.Hidden,
            });
            p?.WaitForExit(20_000);
            if (p?.ExitCode == 0) System.IO.File.Delete(guard);
        }
        catch { } // UAC declined: both stay (the rule only allows the group subnets; the ports only skip group gateways)
    }

    private static bool RuleExists()
    {
        using var p = Process.Start(new ProcessStartInfo("netsh", $"advfirewall firewall show rule name=\"{RuleName}\"")
        {
            UseShellExecute = false, CreateNoWindow = true, RedirectStandardOutput = true,
        });
        if (p == null) return false;
        p.StandardOutput.ReadToEnd();
        p.WaitForExit(5000);
        return p.ExitCode == 0;
    }
}
