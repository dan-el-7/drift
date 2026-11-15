using System.Numerics;
using Microsoft.Graphics.Canvas;
using Microsoft.Graphics.Canvas.Effects;
using Microsoft.Graphics.Canvas.Geometry;
using Microsoft.Graphics.Canvas.Text;
using Windows.Foundation;
using Windows.UI;
using static Drift.Stage.Sp;

namespace Drift.Stage;

public readonly record struct Palette(Color Ink, Color Paper, Color G1, Color G2);

public interface IStageRenderer
{
    void Draw(CanvasDrawingSession ds, ICanvasResourceCreatorWithDpi rc, StageModel m, Palette c, double dt);
}

public static class Fonts
{
    /// Bundled Geist for XAML and Win2D text (ms-appx resolves to the app folder).
    public const string Geist = "ms-appx:///Assets/Fonts/Geist-Variable.ttf#Geist";
    public const string GeistMono = "ms-appx:///Assets/Fonts/GeistMono-Variable.ttf#Geist Mono";

    /// Win2D can't resolve ms-appx when unpackaged; a file: URI to the bundled font works there.
    /// If a format is ever rejected, `Draw` falls back to the system UI font (never crash on text).
    public static string Win2DGeist = new Uri(System.IO.Path.Combine(AppContext.BaseDirectory, "Assets", "Fonts", "Geist-Variable.ttf")).AbsoluteUri + "#Geist";
    public static void Win2DFallback() => Win2DGeist = "Segoe UI Variable Display";
}

/// Real throughput -> on-screen speed (motion.json throughput_visual: log scale, 24..640 px/s).
internal static class Speed
{
    public static double PxPerSec(double mbps) => 24 + (640 - 24) * Math.Min(1, Math.Log(1 + mbps) / Math.Log(1 + 120));
}

/// Ink (default look): everything is one drop of ink. White shapes are blurred, summed and
/// thresholded (contrast ×12 around 50%), so nearby shapes fuse and thin ones evaporate; the
/// result is mapped to the theme's ink/paper colours. Same recipe as the prototype's CSS filter.
public sealed class InkRenderer : IStageRenderer
{
    /// The prototype's canvas blur is in device pixels; the look was approved on a 125% screen, so
    /// its 7 px blur is 5.6 DIPs. Keeping that ratio keeps the drops as fat and goofy as approved.
    private const float BlurScale = 0.8f;
    /// The threshold's steepness. 32 cut edges to ~0.5 px (jagged); ~12 keeps the same 50% cut (same
    /// shapes) with about a pixel of falloff: anti-aliased, still crisp.
    private const float InkContrast = 12f, InkOffset = -(InkContrast - 1) / 2;
    private CanvasRenderTarget? _rt;
    private readonly List<double> _ripples = new();
    private double _lastRipple = -9;
    private readonly List<(double y, double r)> _drops = new();
    private readonly Random _rnd = new();
    private readonly InkPin _pin = new();

    public void Draw(CanvasDrawingSession ds, ICanvasResourceCreatorWithDpi rc, StageModel m, Palette c, double dt)
    {
        if (_rt == null || Math.Abs(_rt.Size.Width - m.W) > 0.5 || Math.Abs(_rt.Size.Height - m.H) > 0.5 || _rt.Dpi != rc.Dpi)
        {
            _rt?.Dispose();
            _rt = new CanvasRenderTarget(rc, (float)m.W, (float)m.H, rc.Dpi);
        }
        float cx = (float)m.Cx, cy = (float)m.Cy;
        var white = Microsoft.UI.Colors.White;
        var T = m.T;

        // --- shapes blurred at 7 px
        var cl7 = new CanvasCommandList(rc);
        using (var d = cl7.CreateDrawingSession())
        {
            var breath = m.Reduced ? 1 : 1 + 0.25 * (0.5 - 0.5 * Math.Cos(2 * Math.PI * T / 2.4));
            Disc(d, cx, cy, 12 * breath * m.Core.X);
            // lava-lamp satellites: full orbit idle, tighter elsewhere, same drop size (they keep fusing)
            if (!m.Reduced && m.Core.X > 0.02)
                for (var i = 0; i < 3; i++)
                {
                    var a = T * (0.55 + i * 0.23) + i * 2.1;
                    var rr = (20 + 9 * Math.Sin(T * 0.8 + i)) * m.Core.X * (m.Mode == StageMode.Idle ? 1 : 0.6);
                    Disc(d, cx + Math.Cos(a) * rr, cy + Math.Sin(a) * rr, (4.5 + i) * m.Core.X);
                }
            // presence ripples: liquid rings that thin until they evaporate
            if (m.Sonar.X > 0.05 && !m.Reduced && T - _lastRipple > 2.8) { _ripples.Add(T); _lastRipple = T; }
            _ripples.RemoveAll(t0 => T - t0 >= 2.6);
            foreach (var t0 in _ripples)
            {
                var u = (T - t0) / 2.6;
                var lw = 9 * Math.Pow(1 - u, 1.3) * Math.Min(1, m.Sonar.X * 1.4);
                if (lw > 0.2) d.DrawCircle(cx, cy, (float)(16 + u * 110), white, (float)lw);
            }
            // nearby devices pinch off the core (below ~9 px radius a drop falls under the threshold).
            // Small drops have their own life: a breathing wobble, a bob, a satellite that fuses in and out.
            void Goofy(double x, double y, double r, double ph)
            {
                if (r < 0.3) return;
                if (m.Reduced) { Disc(d, x, y, r); return; }
                var bob = 2.2 * Math.Sin(T * 1.3 + ph);
                Disc(d, x + 1.5 * Math.Sin(T * 0.9 + ph * 2), y + bob, r * (1 + 0.12 * Math.Sin(T * 1.7 + ph)));
                var a = T * (1.1 + 0.15 * (ph % 3)) + ph;
                var rr = (12 + 5 * Math.Sin(T * 0.9 + ph)) * (r / 11);
                Disc(d, x + Math.Cos(a) * rr, y + bob + Math.Sin(a) * rr, 4.6 * (r / 11));
            }
            for (var i = 0; i < m.Nodes.Count; i++)
            {
                var n = m.Nodes[i];
                Goofy(cx + (n.Tx - cx) * n.K.X, cy + (n.Ty - cy) * n.K.X, 11 * n.A.X, 1.7 + i * 2.3);
            }
            // the other phone
            Goofy(cx, m.PeerY.X, 10 * m.Peer.X, 0.4);
            // droplets feeding the ring at the real speed
            if (m.Ring.X > 0.02 && !m.Reduced && m.Mode == StageMode.Ring)
            {
                var (ro, _) = StageModel.RingGeo(Clamp(m.P.X));
                var sp = Speed.PxPerSec(m.Rate);
                if (_rnd.NextDouble() < dt * (m.Rate < 1 ? 1.2 : 9)) _drops.Add((m.PeerY.X + 6, 3 + _rnd.NextDouble() * 2));
                for (var i = 0; i < _drops.Count; i++) _drops[i] = (_drops[i].y + sp * dt * 0.75, _drops[i].r);
                _drops.RemoveAll(q => q.y >= cy - ro + 4);
                foreach (var q in _drops) Disc(d, cx + Math.Sin(q.y * 0.05) * 2, q.y, q.r);
            }
            else _drops.Clear();
            // the puff, with a living edge
            if (m.Puff.X > 0.5)
            {
                using var pb = new CanvasPathBuilder(rc);
                for (var i = 0; i <= 120; i++)
                {
                    var a = i / 120.0 * Math.PI * 2;
                    var wob = m.Reduced ? 0 : 7 * Math.Sin(6 * a + T * 3) * Math.Min(1, 60 / m.Puff.X);
                    var r = m.Puff.X + wob;
                    var pt = new Vector2((float)(cx + Math.Cos(a) * r), (float)(cy + Math.Sin(a) * r));
                    if (i == 0) pb.BeginFigure(pt); else pb.AddLine(pt);
                }
                pb.EndFigure(CanvasFigureLoop.Closed);
                using var g = CanvasGeometry.CreatePath(pb);
                d.FillGeometry(g, white);
            }
        }

        // --- the ring, blurred less (a thin stroke would vanish under the threshold)
        CanvasCommandList? cl4 = null;
        if (m.Ring.X > 0.02)
        {
            cl4 = new CanvasCommandList(rc);
            using var d = cl4.CreateDrawingSession();
            var p = Clamp(m.P.X);
            var (ro, ri) = StageModel.RingGeo(p);
            var rcen = (ro + ri) / 2;
            var lw = Math.Max(12, ro - ri) * Clamp(m.Ring.X);
            using var pb = new CanvasPathBuilder(rc);
            for (var i = 0; i <= 96; i++)
            {
                var a = i / 96.0 * Math.PI * 2;
                var wob = m.Reduced ? 0 : 1.8 * Math.Sin(5 * a + T * 2.2) * (1 - p);
                var pt = new Vector2((float)(cx + Math.Cos(a) * (rcen + wob) * m.RingS.X), (float)(cy + Math.Sin(a) * (rcen + wob) * m.RingS.X));
                if (i == 0) pb.BeginFigure(pt); else pb.AddLine(pt);
            }
            pb.EndFigure(CanvasFigureLoop.Closed);
            using var g = CanvasGeometry.CreatePath(pb);
            d.DrawGeometry(g, white, (float)lw);
        }

        // --- sum, threshold, colour
        using (var d = _rt.CreateDrawingSession())
        {
            d.Clear(Microsoft.UI.Colors.Black);
            var all = new Rect(0, 0, m.W, m.H);
            void Add(ICanvasImage img, float blur)
            {
                using var b = new GaussianBlurEffect { Source = img, BlurAmount = blur, BorderMode = EffectBorderMode.Soft };
                d.DrawImage(b, Vector2.Zero, all, 1f, CanvasImageInterpolation.Linear, CanvasComposite.Add);
            }
            Add(cl7, 7 * BlurScale);
            if (cl4 != null) Add(cl4, 4 * BlurScale);
        }
        cl7.Dispose();
        cl4?.Dispose();

        using var contrast = new ColorMatrixEffect
        {
            Source = _rt,
            ColorMatrix = new Matrix5x4 { M11 = InkContrast, M22 = InkContrast, M33 = InkContrast, M44 = 1, M51 = InkOffset, M52 = InkOffset, M53 = InkOffset },
            ClampOutput = true,
        };
        float dr = (c.Ink.R - c.Paper.R) / 255f, dg = (c.Ink.G - c.Paper.G) / 255f, db = (c.Ink.B - c.Paper.B) / 255f;
        using var map = new ColorMatrixEffect
        {
            Source = contrast,
            // white -> ink, black -> paper (input R drives all three channels)
            ColorMatrix = new Matrix5x4 { M11 = dr, M12 = dg, M13 = db, M44 = 1, M51 = c.Paper.R / 255f, M52 = c.Paper.G / 255f, M53 = c.Paper.B / 255f },
            ClampOutput = true,
        };
        ds.DrawImage(map);

        // --- the code: outlines drawing on, then the digits focusing in (see InkPin)
        _pin.Draw(ds, rc, m, c);
    }

    private static void Disc(CanvasDrawingSession d, double x, double y, double r)
    {
        if (r > 0.3) d.FillCircle((float)x, (float)y, (float)r, Microsoft.UI.Colors.White);
    }
}

/// Monolith (optional look): precise and typographic; hairlines, a tick dial, particles on a line.
public sealed class MonolithRenderer : IStageRenderer
{
    private readonly List<double> _parts = new();
    private readonly Random _rnd = new();

    public void Draw(CanvasDrawingSession ds, ICanvasResourceCreatorWithDpi rc, StageModel m, Palette c, double dt)
    {
        float cx = (float)m.Cx, cy = (float)m.Cy;
        const double R = StageModel.R;
        var T = m.T;
        ds.Clear(c.Paper);
        if (m.Sonar.X > 0.01 && !m.Reduced)
            for (var j = 0; j < 3; j++)
            {
                var u = (T / 2.8 + j / 3.0) % 1;
                ds.DrawCircle(cx, cy, (float)(8 + u * 116), A(c.G1, 0.5 * Math.Pow(1 - u, 1.6) * m.Sonar.X), 1);
            }
        foreach (var n in m.Nodes)
        {
            if (n.A.X < 0.01) continue;
            float x = (float)(cx + (n.Tx - cx) * n.K.X), y = (float)(cy + (n.Ty - cy) * n.K.X);
            ds.DrawLine(cx, cy, x, y, A(c.G2, n.A.X), 1);
            ds.FillCircle(x, y, 4.5f, A(c.Ink, n.A.X));
        }
        if (m.Peer.X > 0.01)
        {
            var ny = m.PeerY.X;
            var (ro, _) = StageModel.RingGeo(Clamp(m.P.X));
            var end = m.Mode == StageMode.Ring ? cy - ro - 22 * m.Dial.X : cy - 12;
            var a = Clamp(m.Peer.X);
            ds.DrawLine(cx, (float)(ny + 7), cx, (float)Math.Max(ny + 7, end), A(c.G2, a), 1);
            ds.FillCircle(cx, (float)ny, 4, A(c.Ink, a));
            if (m.Mode == StageMode.Ring && !m.Reduced)
            {
                var sp = Speed.PxPerSec(m.Rate);
                if (_rnd.NextDouble() < dt * (m.Rate < 1 ? 2 : 22)) _parts.Add(ny + 8);
                for (var i = 0; i < _parts.Count; i++) _parts[i] += sp * dt;
                _parts.RemoveAll(y => y >= end);
                foreach (var y in _parts) ds.FillRectangle(cx - 0.75f, (float)y, 1.5f, m.Rate < 1 ? 3 : 7, A(c.Ink, a));
            }
            else _parts.Clear();
        }
        if (m.Core.X > 0.01)
        {
            var b = m.Reduced ? 1 : 1 + 0.42 * (0.5 - 0.5 * Math.Cos(2 * Math.PI * T / 2.4));
            ds.FillCircle(cx, cy, (float)(5 * b * m.Core.X), c.Ink);
        }
        if (m.Dial.X > 0.01)
            for (var i = 0; i < 72; i++)
            {
                var a = -Math.PI / 2 + i / 72.0 * Math.PI * 2;
                var on = i / 72.0 < m.P.X;
                double r0 = R + 14, r1 = R + (i % 6 == 0 ? 21 : 18);
                ds.DrawLine((float)(cx + Math.Cos(a) * r0), (float)(cy + Math.Sin(a) * r0), (float)(cx + Math.Cos(a) * r1), (float)(cy + Math.Sin(a) * r1),
                    A(on ? c.Ink : c.G2, m.Dial.X), 1);
            }
        if (m.Ring.X > 0.01)
        {
            var (ro, ri) = StageModel.RingGeo(Clamp(m.P.X));
            var s = m.RingS.X;
            ds.DrawCircle(cx, cy, (float)((ro + ri) / 2 * s), A(c.Ink, Clamp(m.Ring.X)), (float)(Math.Max(1.5, ro - ri) * s));
        }
        if (m.Puff.X > 0.5) ds.FillCircle(cx, cy, (float)m.Puff.X, c.Ink);
        DrawPin(ds, rc, m, c);
    }

    // The code, crisp type resolving from noise (prototype pinTick): grey digits flicker under a
    // light blur, then each locks in turn, sharpening from a 5px blur as it drops 8px into place.
    private readonly string[] _cell = ["", "", "", ""];
    private CanvasTextFormat? _pinFormat;
    private double _pinSeen = double.NaN;

    private void DrawPin(CanvasDrawingSession ds, ICanvasResourceCreatorWithDpi rc, StageModel m, Palette c)
    {
        if (m.Pin is not { } pin || m.Digits.X <= 0.01) return;
        _pinFormat ??= new CanvasTextFormat
        {
            FontFamily = Fonts.Win2DGeist, FontSize = 88, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            HorizontalAlignment = CanvasHorizontalAlignment.Center, VerticalAlignment = CanvasVerticalAlignment.Center,
        };
        if (m.PinStart != _pinSeen) { _pinSeen = m.PinStart; Array.Fill(_cell, ""); }
        var ms = (m.T - m.PinStart) * 1000;
        var alpha = Clamp(m.Digits.X);
        var n = Math.Min(4, pin.Length);
        for (var i = 0; i < n; i++)
        {
            double blur, dy = 0;
            Color col;
            if (m.Reduced || ms >= 260 + i * 120)
            {
                var u = Clamp((ms - 260 - i * 120) / 260);
                _cell[i] = pin[i].ToString();
                col = c.Ink;
                blur = m.Reduced ? 0 : (1 - u) * 5;
                dy = m.Reduced ? 0 : (1 - u) * (1 - u) * 8;
            }
            else
            {
                if (_rnd.NextDouble() < 0.6 || _cell[i].Length == 0) _cell[i] = _rnd.Next(10).ToString();
                col = c.G1;
                blur = 1.5;
            }
            // 56px cells centred on the window; letter-spacing -0.05em nudges each glyph 2.2px right.
            var x = m.Cx - 2 * 56 + i * 56 + 28 + 2.2;
            var rect = new Rect(x - 40, m.Cy - 60 + dy, 80, 120);
            using var cl = new CanvasCommandList(rc);
            using (var d = cl.CreateDrawingSession())
            {
                try { d.DrawText(_cell[i], rect, A(col, alpha), _pinFormat); }
                catch (ArgumentException) { Fonts.Win2DFallback(); _pinFormat = null; return; }
            }
            if (blur < 0.05) ds.DrawImage(cl);
            else
            {
                // CSS blur on a 125% screen matches Win2D at 0.8x (measured against the prototype).
                using var b = new GaussianBlurEffect { Source = cl, BlurAmount = (float)(blur * 0.8), BorderMode = EffectBorderMode.Soft };
                ds.DrawImage(b);
            }
        }
    }

    internal static Color A(Color col, double a) => Color.FromArgb((byte)(255 * Clamp(a)), col.R, col.G, col.B);
}

/// Field (optional look): the window is a dot matrix and nothing else is drawn.
public sealed class FieldRenderer : IStageRenderer
{
    private const int Sp = 10, Cols = 38, Rows = 52, Ox = 5, Oy = 5;
    private readonly Random _rnd = new();
    private readonly List<double> _pulses = new();
    private Dictionary<int, double> _pinCells = new();
    private string? _pinFor;

    private static readonly Dictionary<char, string[]> Glyph = new()
    {
        ['0'] = ["01110", "10001", "10011", "10101", "11001", "10001", "01110"], ['1'] = ["00100", "01100", "00100", "00100", "00100", "00100", "01110"],
        ['2'] = ["01110", "10001", "00001", "00010", "00100", "01000", "11111"], ['3'] = ["11110", "00001", "00001", "01110", "00001", "00001", "11110"],
        ['4'] = ["00010", "00110", "01010", "10010", "11111", "00010", "00010"], ['5'] = ["11111", "10000", "11110", "00001", "00001", "10001", "01110"],
        ['6'] = ["00110", "01000", "10000", "11110", "10001", "10001", "01110"], ['7'] = ["11111", "00001", "00010", "00100", "01000", "01000", "01000"],
        ['8'] = ["01110", "10001", "10001", "01110", "10001", "10001", "01110"], ['9'] = ["01110", "10001", "10001", "01111", "00001", "00010", "01100"],
    };

    private void BuildPin(string pin, double cy)
    {
        _pinCells = new();
        const int gw = 4 * 5 + 3 * 2;
        int c0 = (int)Math.Round((Cols - gw) / 2.0), r0 = (int)Math.Round((cy - Oy) / Sp) - 3;
        for (var gi = 0; gi < Math.Min(4, pin.Length); gi++)
        {
            if (!Glyph.TryGetValue(pin[gi], out var rows)) continue;
            for (var ry = 0; ry < 7; ry++)
                for (var rx = 0; rx < 5; rx++)
                    if (rows[ry][rx] == '1') _pinCells[(r0 + ry) * Cols + c0 + gi * 7 + rx] = 0.12 * gi + _rnd.NextDouble() * 0.35;
        }
        _pinFor = pin;
    }

    public void Draw(CanvasDrawingSession ds, ICanvasResourceCreatorWithDpi rc, StageModel m, Palette c, double dt)
    {
        double cx = m.Cx, cy = m.Cy, T = m.T;
        ds.Clear(c.Paper);
        var p = Clamp(m.P.X);
        var (ro, ri) = StageModel.RingGeo(p, 4);
        var ny = m.PeerY.X;
        var br = m.Reduced ? 1 : 0.5 - 0.5 * Math.Cos(2 * Math.PI * T / 2.4);
        if (m.Pin != null && m.Pin != _pinFor) BuildPin(m.Pin, cy);
        var pinT = m.Pin != null ? T - m.PinStart : 0;
        if (m.Mode == StageMode.Ring && !m.Reduced)
        {
            var sp = Speed.PxPerSec(m.Rate);
            if (_rnd.NextDouble() < dt * (m.Rate < 1 ? 1.5 : 14)) _pulses.Add(ny + 10);
            for (var i = 0; i < _pulses.Count; i++) _pulses[i] += sp * dt;
            _pulses.RemoveAll(y => y >= cy - ro);
        }
        else _pulses.Clear();
        var nodes = m.Nodes.Select(n => (X: cx + (n.Tx - cx) * n.K.X, Y: cy + (n.Ty - cy) * n.K.X, a: n.A.X)).ToArray();

        for (var r = 0; r < Rows; r++)
            for (var col = 0; col < Cols; col++)
            {
                double X = Ox + col * Sp, Y = Oy + r * Sp, dx = X - cx, d = Math.Sqrt(dx * dx + (Y - cy) * (Y - cy));
                var b = m.Core.X * (d < 6 ? 1 : d < 16 ? 0.35 + 0.4 * br : 0);
                if (m.Sonar.X > 0.01 && !m.Reduced)
                    for (var j = 0; j < 3; j++)
                    {
                        var u = (T / 2.8 + j / 3.0) % 1;
                        var rr = 10 + u * 150;
                        b = Math.Max(b, m.Sonar.X * 0.75 * Math.Pow(1 - u, 1.4) * Math.Exp(-Math.Pow((d - rr) / 6, 2)));
                    }
                foreach (var n in nodes) if (n.a > 0.01 && Math.Sqrt((X - n.X) * (X - n.X) + (Y - n.Y) * (Y - n.Y)) < 11) b = Math.Max(b, n.a);
                if (m.Peer.X > 0.01)
                {
                    if (Math.Sqrt(dx * dx + (Y - ny) * (Y - ny)) < 9) b = Math.Max(b, m.Peer.X);
                    if (Math.Abs(dx) < 1 && Y > ny + 8 && Y < cy - (m.Mode == StageMode.Ring ? ro : 10))
                    {
                        var v = 0.28;
                        foreach (var py in _pulses) v = Math.Max(v, Math.Exp(-Math.Pow((Y - py) / 7, 2)));
                        b = Math.Max(b, v * m.Peer.X);
                    }
                }
                if (m.Digits.X > 0.01)
                {
                    if (_pinCells.TryGetValue(r * Cols + col, out var at))
                        b = Math.Max(b, m.Digits.X * (m.Reduced || pinT > at + 0.25 ? 1 : 0.15 + 0.5 * _rnd.NextDouble()));
                    else if (!m.Reduced && pinT < 0.9 && Math.Abs(Y - cy) < 48 && Math.Abs(dx) < 140 && _rnd.NextDouble() < 0.06 * (1 - pinT / 0.9))
                        b = Math.Max(b, 0.5 * m.Digits.X);
                }
                if (m.Ring.X > 0.01)
                {
                    var e = Math.Min(d - ri + 5, ro - d + 5) / 5;
                    if (e > 0) b = Math.Max(b, m.Ring.X * Math.Min(1, e));
                }
                var rad = 1.1;
                if (m.Puff.X > 0.5 && d < m.Puff.X) { b = 1; rad = 1.1 + (Sp * 0.62 - 1.1) * Clamp(m.Fill.X); }
                var lit = b > 0.02;
                var colr = lit ? MonolithRenderer.A(c.Ink, 0.25 + 0.75 * Math.Min(1, b)) : c.G2;
                ds.FillCircle((float)X, (float)Y, (float)(lit ? rad + 0.5 * Math.Min(1, b) : rad * 0.85), colr);
            }
    }
}

/// The Ink code (prototype: drawInkPin). All four digits draw on together as outlines (Geist 600's
/// real contours, one smooth stroke) with a soft breathing wobble; then the solid digits come into
/// focus with a small springy bounce as the outlines fade. Every digit floats on its own slow bob and
/// sway throughout. The focus blur is rendered once per code (a few levels, crossfaded), so no frame
/// blurs anything live.
internal sealed class InkPin
{
    private const double First = 120, Stagger = 45, DrawMs = 1050, Gap = 60, RevealMs = 420, Wobble = 0.8;
    private const float LineW = 1.8f, Size = 84, Cap = 710f / 1000f * Size, Cell = 110, BlurScale = 0.8f;
    private static readonly float[] Focus = [6, 3.5f, 1.6f, 0];

    private sealed class Digit
    {
        public readonly List<(Vector2[] pts, Vector2[] nrm)> Contours = new();
        public CanvasRenderTarget[] Levels = [];
        public Vector2 Centre;
        public double Delay, Phase;
    }

    private List<Digit> _digits = new();
    private string? _for;
    private Color _ink;
    private float _dpi;
    private CanvasStrokeStyle? _round;

    private static double EaseDraw(double t) => t < 0.5 ? 4 * t * t * t : 1 - Math.Pow(-2 * t + 2, 3) / 2;

    private void Build(ICanvasResourceCreatorWithDpi rc, StageModel m, string pin, Color ink)
    {
        foreach (var d in _digits) foreach (var l in d.Levels) l.Dispose();
        _digits = new();
        _for = pin; _ink = ink; _dpi = rc.Dpi;
        CanvasTextFormat Format() => new() { FontFamily = Fonts.Win2DGeist, FontSize = Size, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, WordWrapping = CanvasWordWrapping.NoWrap };
        var fmt = Format();
        for (var i = 0; i < Math.Min(4, pin.Length); i++)
        {
            CanvasTextLayout layout;
            try { layout = new CanvasTextLayout(rc, pin[i].ToString(), fmt, 0, 0); }
            catch (ArgumentException) { Fonts.Win2DFallback(); fmt = Format(); layout = new CanvasTextLayout(rc, pin[i].ToString(), fmt, 0, 0); }
            using (layout)
            {
                var adv = (float)layout.LayoutBounds.Width;
                var baseline = layout.LineMetrics.Length > 0 ? layout.LineMetrics[0].Baseline : Size * 0.9f;
                var centre = new Vector2((float)(m.Cx - 96 + i * 64), (float)(m.Cy + 2));
                var origin = new Vector2(centre.X - adv / 2, centre.Y + Cap / 2 - baseline);
                using var raw = CanvasGeometry.CreateText(layout).Transform(Matrix3x2.CreateTranslation(origin));
                // Geist's outlines overlap at some joins (8, 6, 5); filled as-is the overlaps cancel into
                // tiny holes. One merged outline fills and traces cleanly.
                var whole = new FigureSplitter(rc);
                raw.SendPathTo(whole);
                using var winding = whole.Whole();
                using var glyph = winding.Outline();
                var d = new Digit { Centre = centre, Delay = i * Stagger, Phase = i * 1.7 };
                // outline contours, sampled every 1.5 px with their normals
                var split = new FigureSplitter(rc);
                glyph.SendPathTo(split);
                foreach (var fig in split.Figures)
                {
                    using (fig)
                    {
                        var len = fig.ComputePathLength();
                        var n = Math.Max(12, (int)Math.Ceiling(len / 1.5f));
                        var pts = new Vector2[n + 1];
                        var nrm = new Vector2[n + 1];
                        for (var j = 0; j <= n; j++)
                        {
                            pts[j] = fig.ComputePointOnPath(len * j / n, out var tan);
                            var nv = new Vector2(-tan.Y, tan.X);
                            nrm[j] = nv.LengthSquared() > 0 ? Vector2.Normalize(nv) : Vector2.Zero;
                        }
                        d.Contours.Add((pts, nrm));
                    }
                }
                // the solid digit at each focus level, rendered once into its own cell
                using var cellGlyph = glyph.Transform(Matrix3x2.CreateTranslation(Cell / 2 - centre.X, Cell / 2 - centre.Y));
                d.Levels = Focus.Select(b =>
                {
                    var rt = new CanvasRenderTarget(rc, Cell, Cell, rc.Dpi);
                    using var ds = rt.CreateDrawingSession();
                    ds.Clear(Microsoft.UI.Colors.Transparent);
                    if (b <= 0) ds.FillGeometry(cellGlyph, ink);
                    else
                    {
                        using var cl = new CanvasCommandList(rc);
                        using (var g = cl.CreateDrawingSession()) g.FillGeometry(cellGlyph, ink);
                        using var blur = new GaussianBlurEffect { Source = cl, BlurAmount = b * BlurScale, BorderMode = EffectBorderMode.Soft };
                        ds.DrawImage(blur);
                    }
                    return rt;
                }).ToArray();
                _digits.Add(d);
            }
        }
    }

    public void Draw(CanvasDrawingSession ds, ICanvasResourceCreatorWithDpi rc, StageModel m, Palette c)
    {
        if (m.Pin is not { } pin || m.Digits.X <= 0.02) return;
        if (_for != pin || _ink != c.Ink || _dpi != rc.Dpi) Build(rc, m, pin, c.Ink);
        _round ??= new CanvasStrokeStyle { StartCap = CanvasCapStyle.Round, EndCap = CanvasCapStyle.Round, LineJoin = CanvasLineJoin.Round };
        var a0 = Sp.Clamp(m.Digits.X);
        var T = m.T;
        var t = m.Reduced ? 1e9 : (T - m.PinStart) * 1000;
        var drawn = First + 3 * Stagger + DrawMs;
        var u = Sp.Clamp((t - drawn - Gap) / RevealMs); // the reveal, 0..1
        var saved = ds.Transform;
        foreach (var d in _digits)
        {
            // each digit floats: a slow bob and a slight sway of its own, before and after it fills
            var float_ = m.Reduced ? Matrix3x2.Identity
                : Matrix3x2.CreateRotation((float)(0.02 * Math.Sin(T * 0.8 + d.Phase * 1.3)), d.Centre)
                  * Matrix3x2.CreateTranslation(0, (float)(1.6 * Math.Sin(T * 1.25 + d.Phase)));
            ds.Transform = float_ * saved;

            // 1. the outline drawing on (all digits together, a beat apart), breathing as it goes
            if (u < 1)
            {
                var p = EaseDraw(Sp.Clamp((t - First - d.Delay) / DrawMs));
                if (p > 0)
                {
                    var calm = 1 - Sp.Clamp(u * 1.6); // the wobble settles as the digit fills
                    var col = MonolithRenderer.A(c.Ink, a0 * (1 - Sp.Clamp(u / 0.7)));
                    foreach (var (pts, nrm) in d.Contours)
                    {
                        var last = Math.Max(1, (int)Math.Floor(p * (pts.Length - 1)));
                        using var pb = new CanvasPathBuilder(rc);
                        for (var j = 0; j <= last; j++)
                        {
                            var w = m.Reduced ? 0 : (float)(Wobble * calm * Math.Sin(j / 6.0 + T * 5 + d.Phase));
                            var q = pts[j] + nrm[j] * w;
                            if (j == 0) pb.BeginFigure(q); else pb.AddLine(q);
                        }
                        pb.EndFigure(CanvasFigureLoop.Open);
                        using var g = CanvasGeometry.CreatePath(pb);
                        ds.DrawGeometry(g, col, LineW, _round);
                    }
                }
            }

            // 2. the solid digit comes into focus (pre-blurred levels, crossfaded) with a little bounce
            if (u > 0)
            {
                var e = 1 - Math.Pow(1 - u, 3);
                var f = e * (Focus.Length - 1);
                var i0 = (int)Math.Floor(f);
                var i1 = Math.Min(Focus.Length - 1, i0 + 1);
                var mix = f - i0;
                var tt = u * RevealMs / 1000;
                var bounce = m.Reduced ? 1 : 1 + 0.06 * Math.Exp(-7 * tt) * Math.Cos(16 * tt) - 0.06 * Math.Exp(-40 * tt); // springy settle
                ds.Transform = Matrix3x2.CreateScale((float)bounce, d.Centre) * float_ * saved;
                var at = d.Centre - new Vector2(Cell / 2, Cell / 2);
                var src = new Rect(0, 0, Cell, Cell);
                ds.DrawImage(d.Levels[i0], at, src, (float)(a0 * e * (1 - mix)));
                if (mix > 0) ds.DrawImage(d.Levels[i1], at, src, (float)(a0 * e * mix));
            }
        }
        ds.Transform = saved;
    }

    /// Splits a glyph's geometry into one geometry per contour (so each traces on its own).
    private sealed class FigureSplitter(ICanvasResourceCreator rc) : ICanvasPathReceiver
    {
        public readonly List<CanvasGeometry> Figures = new();
        private CanvasPathBuilder? _pb;
        private CanvasPathBuilder? _all; // every figure, filled non-zero (as text rendering does)
        private CanvasPathBuilder All() { if (_all == null) { _all = new CanvasPathBuilder(rc); _all.SetFilledRegionDetermination(CanvasFilledRegionDetermination.Winding); } return _all; }
        /// All figures as one geometry with the non-zero rule: overlaps stay solid instead of cancelling.
        public CanvasGeometry Whole() { var g = CanvasGeometry.CreatePath(All()); _all!.Dispose(); _all = null; return g; }
        public void BeginFigure(Vector2 startPoint, CanvasFigureFill figureFill) { _pb = new CanvasPathBuilder(rc); _pb.BeginFigure(startPoint); All().BeginFigure(startPoint); }
        public void AddArc(Vector2 endPoint, float radiusX, float radiusY, float rotationAngle, CanvasSweepDirection sweepDirection, CanvasArcSize arcSize) { _pb?.AddArc(endPoint, radiusX, radiusY, rotationAngle, sweepDirection, arcSize); _all?.AddArc(endPoint, radiusX, radiusY, rotationAngle, sweepDirection, arcSize); }
        public void AddCubicBezier(Vector2 controlPoint1, Vector2 controlPoint2, Vector2 endPoint) { _pb?.AddCubicBezier(controlPoint1, controlPoint2, endPoint); _all?.AddCubicBezier(controlPoint1, controlPoint2, endPoint); }
        public void AddLine(Vector2 endPoint) { _pb?.AddLine(endPoint); _all?.AddLine(endPoint); }
        public void AddQuadraticBezier(Vector2 controlPoint, Vector2 endPoint) { _pb?.AddQuadraticBezier(controlPoint, endPoint); _all?.AddQuadraticBezier(controlPoint, endPoint); }
        public void SetFilledRegionDetermination(CanvasFilledRegionDetermination filledRegionDetermination) { }
        public void SetSegmentOptions(CanvasFigureSegmentOptions figureSegmentOptions) { }
        public void EndFigure(CanvasFigureLoop figureLoop)
        {
            _all?.EndFigure(CanvasFigureLoop.Closed);
            if (_pb == null) return;
            _pb.EndFigure(CanvasFigureLoop.Closed);
            Figures.Add(CanvasGeometry.CreatePath(_pb));
            _pb.Dispose();
            _pb = null;
        }
    }
}
