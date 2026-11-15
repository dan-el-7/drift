using System.Text.Json;
using Microsoft.UI.Composition;
using Windows.UI;

namespace Drift.Design;

/// A spring as the designer tunes it: damping ratio + response (undamped period).
/// Same mapping as the drift-design crate: Composition takes these directly.
public readonly record struct Spring(float DampingRatio, TimeSpan Period);

/// design/motion.json + design/tokens.json, loaded once. One source of truth with Linux.
public static class Design
{
    public static readonly Dictionary<string, Spring> Springs = new();
    public static TimeSpan SonarRing = TimeSpan.FromMilliseconds(2800);
    public static int SonarRingCount = 3;
    public static TimeSpan Crossfade = TimeSpan.FromMilliseconds(160);
    public static Color Bg, Fg, Grey1, Grey2;
    public static double PinSize = 56, DisplaySize = 44, TitleSize = 17, BodySize = 13, CaptionSize = 11;
    public static int WindowWidth = 380, WindowHeight = 520;

    /// System "animation effects" setting. When off: no springs, no loops, crossfades only.
    /// `--reduced` forces it on (for testing the still frames against the prototype).
    public static bool ReducedMotion => ForceReduced || !new Windows.UI.ViewManagement.UISettings().AnimationsEnabled;
    public static readonly bool ForceReduced = Environment.GetCommandLineArgs().Contains("--reduced");

    public static Spring Settle => Springs["settle"];
    public static Spring Snappy => Springs["snappy"];
    public static Spring Drop => Springs["drop"];
    public static Spring Breathe => Springs["breathe"];
    public static Spring Puff => Springs["puff"];

    public static void Load(bool dark)
    {
        var dir = Path.Combine(AppContext.BaseDirectory, "design");
        using var motion = JsonDocument.Parse(File.ReadAllText(Path.Combine(dir, "motion.json")));
        foreach (var s in motion.RootElement.GetProperty("springs").EnumerateObject())
            Springs[s.Name] = new Spring((float)s.Value.GetProperty("damping_ratio").GetDouble(),
                TimeSpan.FromMilliseconds(s.Value.GetProperty("response_ms").GetDouble()));
        var loops = motion.RootElement.GetProperty("loops");
        SonarRing = TimeSpan.FromMilliseconds(loops.GetProperty("sonar_ring_ms").GetDouble());
        SonarRingCount = loops.GetProperty("sonar_ring_count").GetInt32();
        Crossfade = TimeSpan.FromMilliseconds(motion.RootElement.GetProperty("reduced_motion").GetProperty("crossfade_ms").GetDouble());

        using var tokens = JsonDocument.Parse(File.ReadAllText(Path.Combine(dir, "tokens.json")));
        var c = tokens.RootElement.GetProperty("color").GetProperty(dark ? "dark" : "light");
        Bg = Hex(c.GetProperty("bg").GetString()!);
        Fg = Hex(c.GetProperty("fg").GetString()!);
        Grey1 = Hex(c.GetProperty("grey1").GetString()!);
        Grey2 = Hex(c.GetProperty("grey2").GetString()!);
        var scale = tokens.RootElement.GetProperty("type").GetProperty("scale_px");
        PinSize = scale.GetProperty("pin").GetDouble();
        DisplaySize = scale.GetProperty("display").GetDouble();
        TitleSize = scale.GetProperty("title").GetDouble();
        BodySize = scale.GetProperty("body").GetDouble();
        CaptionSize = scale.GetProperty("caption").GetDouble();
        var w = tokens.RootElement.GetProperty("window");
        WindowWidth = w.GetProperty("width").GetInt32();
        WindowHeight = w.GetProperty("height").GetInt32();
    }

    private static Color Hex(string s)
    {
        var v = Convert.ToUInt32(s.TrimStart('#'), 16);
        return Color.FromArgb(255, (byte)(v >> 16), (byte)(v >> 8), (byte)v);
    }

    // ---- Composition helpers -------------------------------------------------------------

    public static SpringScalarNaturalMotionAnimation ScalarSpring(Compositor c, Spring s, float to)
    {
        var a = c.CreateSpringScalarAnimation();
        a.DampingRatio = s.DampingRatio;
        a.Period = Period(s);
        a.FinalValue = to;
        return a;
    }

    public static SpringVector3NaturalMotionAnimation Vector3Spring(Compositor c, Spring s, System.Numerics.Vector3 to)
    {
        var a = c.CreateSpringVector3Animation();
        a.DampingRatio = s.DampingRatio;
        a.Period = Period(s);
        a.FinalValue = to;
        return a;
    }

    // Composition's Period behaves like 1/ω (its default, 48 ms, settles in ~0.3 s), so a
    // response of 2π/ω maps to response / 2π. Same as drift-design's `composition()`.
    private static TimeSpan Period(Spring s) => TimeSpan.FromMilliseconds(Math.Max(1, s.Period.TotalMilliseconds / (2 * Math.PI)));
}
