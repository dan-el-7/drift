using Velopack;
using Velopack.Sources;

namespace Drift.Shell;

/// Updates from GitHub Releases (Velopack). Drift only looks (a minute after start, then every 6 hours)
/// and says when there's a new version; nothing is downloaded or installed until the user picks Update.
/// A dev build (not installed by Setup) never checks.
public static class Updates
{
    private const string Repo = "https://github.com/dan-el-7/drift";
    private static UpdateManager? _mgr;
    private static UpdateInfo? _found;

    private static UpdateManager? Manager()
    {
        try { return _mgr ??= new UpdateManager(new GithubSource(Repo, null, false)); }
        catch { return null; }
    }

    /// The installed version, or null for a dev build.
    public static string? Version
    {
        get { try { var m = Manager(); return m is { IsInstalled: true } ? m.CurrentVersion?.ToString() : null; } catch { return null; } }
    }

    /// A newer version that's out, if a check found one.
    public static string? Available => _found?.TargetFullRelease.Version.ToString();

    /// The background check. `found` runs (on any thread) each time a check sees a newer version.
    public static async Task Run(Action<string> found, CancellationToken ct = default)
    {
        var m = Manager();
        if (m is not { IsInstalled: true }) return;
        try { await Task.Delay(TimeSpan.FromMinutes(1), ct); } catch { return; }
        while (!ct.IsCancellationRequested)
        {
            try
            {
                if (await m.CheckForUpdatesAsync() is { } info)
                {
                    _found = info;
                    found(info.TargetFullRelease.Version.ToString());
                }
            }
            catch { /* offline, GitHub down, rate-limited: try again later */ }
            try { await Task.Delay(TimeSpan.FromHours(6), ct); } catch { return; }
        }
    }

    /// The user chose to update: download it, then install and start the new version.
    public static async Task<bool> Install()
    {
        if (_mgr is not { } m || _found is not { } info) return false;
        try
        {
            await m.DownloadUpdatesAsync(info);
            m.ApplyUpdatesAndRestart(info.TargetFullRelease);
            return true;
        }
        catch { return false; }
    }
}
