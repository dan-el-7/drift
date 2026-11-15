namespace Drift.Shell;

/// Where Drift keeps its own files (settings, logs): %AppData%\Drift (Roaming). The install lives
/// in %LocalAppData%\Drift (Velopack); keeping data out of it means the installer never mistakes a
/// settings folder for an existing install, and portable/dev runs never get in its way.
public static class DataDir
{
    public static string Path { get; } = System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "Drift");

    /// One-time move from the old location (%LocalAppData%\Drift, before 0.1.0 installers).
    /// Only Drift's own files are touched; an install there (Update.exe) is left alone.
    public static void Migrate()
    {
        try
        {
            var old = System.IO.Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "Drift");
            if (!Directory.Exists(old)) return;
            Directory.CreateDirectory(Path);
            var settings = System.IO.Path.Combine(old, "settings.json");
            var moved = System.IO.Path.Combine(Path, "settings.json");
            if (File.Exists(settings) && !File.Exists(moved)) File.Move(settings, moved);
            foreach (var f in new[] { "settings.json", "core.log", "crash.log" })
            {
                var p = System.IO.Path.Combine(old, f);
                if (File.Exists(p)) File.Delete(p);
            }
            if (!Directory.EnumerateFileSystemEntries(old).Any()) Directory.Delete(old);
        }
        catch { }
    }
}
