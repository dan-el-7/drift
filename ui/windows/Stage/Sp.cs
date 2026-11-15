using D = Drift.Design.Design;

namespace Drift.Stage;

/// A damped spring stepped per frame: the same solver as the prototype
/// (design/prototype/motion-studies.html) and drift-design's reference, so motion matches.
public sealed class Sp
{
    public double X, V, To;
    private double _z, _r;

    public Sp(double x, string spring = "settle")
    {
        X = To = x;
        Use(spring);
    }

    private void Use(string name)
    {
        var s = D.Springs[name];
        _z = s.DampingRatio;
        _r = s.Period.TotalMilliseconds; // response (undamped period), ms
    }

    public Sp Go(double to, string? spring = null)
    {
        To = to;
        if (spring != null) Use(spring);
        return this;
    }

    public Sp Snap(double x)
    {
        X = To = x;
        V = 0;
        return this;
    }

    public double Step(double dt, bool reduced)
    {
        if (reduced) { X = To; V = 0; return X; }
        var w = 2 * Math.PI / (_r / 1000.0);
        var n = Math.Max(1, (int)Math.Ceiling(dt / 0.004));
        var h = dt / n;
        for (var i = 0; i < n; i++)
        {
            var a = -w * w * (X - To) - 2 * _z * w * V;
            V += a * h;
            X += V * h;
        }
        return X;
    }

    public static double Clamp(double v, double a = 0, double b = 1) => Math.Max(a, Math.Min(b, v));
}
