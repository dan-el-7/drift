namespace Drift.Stage;

public enum StageMode { Idle, Devices, Pin, Ring, Done }

public sealed class StageNode
{
    public required string Id;
    public required string Name;
    public string Kind = "";
    public double Tx, Ty;
    public readonly Sp A = new(0);
    public readonly Sp K = new(0, "drop");
    public object? Tag; // the XAML button for this node
}

/// What the motion layer shows: one pose per screen. Every look draws these springs its own way.
/// Port of the prototype's `pose()`; coordinates are DIPs in a 380×520 window.
public sealed class StageModel
{
    public const double R = 84, Rb = 26;
    public double Cx = 190, Cy = 200, W = 380, H = 520;

    public StageMode Mode { get; private set; } = StageMode.Idle;
    public string? Pin { get; private set; }
    public double PinStart; // seconds on the stage clock
    public double Rate;     // MB/s of the transfer on screen
    public string? PeerName;
    public double T;        // stage clock, seconds
    public bool Reduced;

    public readonly Sp Sonar = new(1), Core = new(1), Peer = new(0), PeerY = new(-40, "drop"),
        Ring = new(0), RingS = new(0.6), Dial = new(0), P = new(0), Puff = new(0, "puff"),
        Fill = new(0), Digits = new(0), Dim = new(1);
    private Sp[] All => [Sonar, Core, Peer, PeerY, Ring, RingS, Dial, P, Puff, Fill, Digits, Dim];

    public readonly List<StageNode> Nodes = new();
    private static readonly double[] Angles = [-90, 25, 155, -25, 205];

    public void Pose(StageMode mode, string? pin = null, string? peer = null, double p = 0, bool dim = false)
    {
        var prev = Mode;
        Mode = mode;
        Dim.Go(dim ? 0.14 : 1);
        Sonar.Go(mode switch { StageMode.Idle => 1, StageMode.Devices => 0.5, _ => 0 });
        Core.Go(mode is StageMode.Idle or StageMode.Devices ? 1 : 0);
        var docked = mode is StageMode.Pin or StageMode.Ring;
        if (docked && Peer.X < 0.05) PeerY.Snap(-30);
        Peer.Go(docked ? 1 : 0);
        PeerY.Go(docked ? Cy - 146 : -40, "drop");
        if (peer != null) PeerName = peer;
        Ring.Go(mode == StageMode.Ring ? 1 : 0);
        RingS.Go(mode == StageMode.Ring ? 1 : 0.6);
        Dial.Go(mode == StageMode.Ring ? 1 : 0);
        if (mode == StageMode.Ring && prev != StageMode.Ring) P.Snap(p);
        Digits.Go(mode == StageMode.Pin ? 1 : 0, "snappy");
        if (mode == StageMode.Pin)
        {
            if (Pin != pin) { Pin = pin; PinStart = T; }
        }
        else Pin = null;
        if (mode == StageMode.Done)
        {
            P.Go(1, "snappy");
            Puff.Snap(Rb).Go(Math.Sqrt(W * W + H * H) * 0.68, "puff");
            Fill.Go(1);
        }
        else
        {
            Puff.Go(0, "settle");
            Fill.Go(0);
        }
        if (mode != StageMode.Devices)
            foreach (var n in Nodes) { n.A.Go(0); n.K.Go(0, "settle"); }
    }

    /// A nearby device: its drop pinches off the centre and settles on an orbit.
    public StageNode AddNode(string id, string name, string kind)
    {
        var n = Nodes.FirstOrDefault(n => n.Id == id);
        if (n == null)
        {
            var a = Angles[Nodes.Count % Angles.Length] * Math.PI / 180;
            n = new StageNode { Id = id, Name = name, Kind = kind, Tx = Cx + Math.Cos(a) * 112, Ty = Cy + Math.Sin(a) * 112 };
            Nodes.Add(n);
        }
        n.A.Go(1);
        n.K.Go(1, "drop");
        return n;
    }

    public void RemoveNode(string id)
    {
        var n = Nodes.FirstOrDefault(n => n.Id == id);
        if (n != null) { n.A.Go(0); n.K.Go(0, "settle"); }
    }

    public void ClearNodes() => Nodes.Clear();

    /// The self-eating ring: outer edge R -> Rb (slow, then the swallow), inner edge -> 0.
    public static (double Ro, double Ri) RingGeo(double p, double w = 1.5) =>
        (R + (Rb - R) * Math.Pow(p, 1.6), (R - w) * (1 - p));

    public void Step(double dt)
    {
        T += dt;
        foreach (var s in All) s.Step(dt, Reduced);
        foreach (var n in Nodes) { n.A.Step(dt, Reduced); n.K.Step(dt, Reduced); }
        Nodes.RemoveAll(n => n.A.To == 0 && n.A.X < 0.01);
    }
}
