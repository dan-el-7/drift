using System.Text.Json;

namespace Drift.Core;

/// Scripted core for design work without a phone. Emits exactly the JSON the Rust core does.
/// Several shares run at once; each starts on Bluetooth (0.2 MB/s) and upgrades to a direct link
/// (~34 MB/s) after 1.2 s, like the real thing. Outgoing sends are accepted after ~2 s.
public sealed class MockCore : ICore
{
    public event Action<CoreEvent>? Event;

    private sealed class Sim
    {
        public ulong Id;
        public string Direction = "incoming";
        public long Total, Done;
        public double T;
        public bool Running, Upgraded;
        public string? SavePath;
        public double AcceptIn = -1; // outgoing: phone accepts after this many seconds
    }

    private readonly object _lock = new();
    private readonly List<Sim> _sims = new();
    private readonly CancellationTokenSource _stop = new();
    private ulong _next = 1;
    private string _dir = "";
    private CancellationTokenSource? _scan;
    private static readonly (string id, string name, string kind)[] Devices =
        [("QCGU", "Dan's phone", "phone"), ("T4B1", "Pixel Tablet", "tablet"), ("AS9X", "Asha's Galaxy", "phone")];

    private void Emit(object e) => Event?.Invoke(CoreEvent.Parse(JsonSerializer.Serialize(e, CoreEvent.Json))!);
    // `--pin 1234` fixes the code (for comparing frames with the prototype).
    private static string Pin()
    {
        var a = Environment.GetCommandLineArgs();
        var i = Array.IndexOf(a, "--pin");
        return i >= 0 && i + 1 < a.Length ? a[i + 1] : Random.Shared.Next(1000, 10000).ToString();
    }

    public void Start(string deviceName, string downloadDir)
    {
        _dir = downloadDir;
        _ = Task.Run(async () =>
        {
            await Task.Delay(200, _stop.Token);
            Emit(new { t = "ready", name = deviceName });
            var clock = System.Diagnostics.Stopwatch.StartNew();
            var last = 0.0;
            while (!_stop.IsCancelled())
            {
                await Task.Delay(100, _stop.Token);
                var now = clock.Elapsed.TotalSeconds;
                var dt = now - last;
                last = now;
                Tick(dt);
            }
        });
    }

    private void Tick(double dt)
    {
        List<object> outbox = new();
        lock (_lock)
        {
            foreach (var s in _sims.ToList())
            {
                if (s.AcceptIn > 0)
                {
                    s.AcceptIn -= dt;
                    if (s.AcceptIn <= 0) { s.Running = true; outbox.Add(new { t = "accepted", share = s.Id, direction = s.Direction }); outbox.Add(new { t = "medium", share = s.Id, medium = "bluetooth" }); }
                }
                if (!s.Running) continue;
                s.T += dt;
                if (!s.Upgraded && s.T >= 1.2) { s.Upgraded = true; outbox.Add(new { t = "medium", share = s.Id, medium = "hotspot", setup_ms = 1200 }); }
                var rate = (s.Upgraded ? 33.9e6 : 0.2e6) * (0.92 + 0.16 * Random.Shared.NextDouble());
                s.Done = Math.Min(s.Total, s.Done + (long)(rate * dt));
                outbox.Add(new { t = "progress", share = s.Id, done = s.Done, total = s.Total, bps = rate });
                if (s.Done >= s.Total)
                {
                    if (s.SavePath != null) outbox.Add(new { t = "saved", share = s.Id, path = s.SavePath, done = s.Total });
                    outbox.Add(new { t = "done", share = s.Id, outcome = "completed" });
                    _sims.Remove(s);
                }
            }
        }
        foreach (var e in outbox) Emit(e);
    }

    private readonly Dictionary<ulong, string> _links = new();

    /// Dev: make the "phone" send something. 1 = a photo, 2 = an album of 24, 3 = a link.
    public void SimulateIncoming(int kind)
    {
        if (kind == 3)
        {
            var lid = Interlocked.Increment(ref _next) - 1;
            lock (_lock) _links[lid] = "https://github.com/google/nearby";
            Emit(new { t = "incoming", share = lid, from = "Dan's phone", kind = "phone", pin = Pin(), files = Array.Empty<FileInfo>(), texts = 1, text_kind = "url", direction = "incoming" });
            return;
        }
        var id = Interlocked.Increment(ref _next) - 1;
        var files = kind == 2
            ? Enumerable.Range(0, 24).Select(i => new FileInfo($"IMG_{4100 + i}.jpg", 38_000_000)).ToArray()
            : [new FileInfo("IMG_20261114_0932.jpg", 148_000_000)];
        lock (_lock) _sims.Add(new Sim { Id = id, Total = files.Sum(f => f.Size), SavePath = Path.Combine(_dir, files.Length == 1 ? files[0].Name : "") });
        Emit(new { t = "incoming", share = id, from = "Dan's phone", kind = "phone", pin = Pin(), files, direction = "incoming" });
    }

    public void Respond(ulong share, bool accept)
    {
        string? link;
        lock (_lock) { if (_links.Remove(share, out link) && !accept) return; }
        if (link != null)
        {
            Emit(new { t = "accepted", share, direction = "incoming" });
            Emit(new { t = "text", share, text = link, text_kind = "url" });
            Emit(new { t = "done", share, outcome = "completed" });
            return;
        }
        lock (_lock)
        {
            var s = _sims.FirstOrDefault(s => s.Id == share);
            if (s == null) return;
            if (!accept) { _sims.Remove(s); return; }
            s.Running = true;
        }
        Emit(new { t = "accepted", share, direction = "incoming" });
        Emit(new { t = "medium", share, medium = "bluetooth" });
    }

    public void Scan(bool on)
    {
        _scan?.Cancel();
        if (!on) return;
        var cts = _scan = new CancellationTokenSource();
        _ = Task.Run(async () =>
        {
            for (var i = 0; i < Devices.Length; i++)
            {
                await Task.Delay(i == 0 ? 650 : 900, cts.Token);
                var (id, name, kind) = Devices[i];
                Emit(new { t = "device_found", id, name, kind });
            }
        }, cts.Token);
    }

    public void Send(string deviceId, IReadOnlyList<string> paths, IReadOnlyList<(string kind, string body)>? texts = null)
    {
        var id = Interlocked.Increment(ref _next) - 1;
        var dev = Devices.FirstOrDefault(d => d.id == deviceId);
        var files = paths.Select(p => new FileInfo(Path.GetFileName(p), File.Exists(p) ? new System.IO.FileInfo(p).Length : 40_000_000)).ToArray();
        _ = Task.Run(async () =>
        {
            await Task.Delay(600, _stop.Token);
            lock (_lock) _sims.Add(new Sim { Id = id, Direction = "outgoing", Total = Math.Max(1, files.Sum(f => f.Size)), AcceptIn = 2.2 });
            Emit(new { t = "waiting", share = id, pin = Pin(), files, direction = "outgoing", name = dev.name ?? "Phone" });
        });
    }

    public void Cancel(ulong share)
    {
        bool had;
        lock (_lock) had = _sims.RemoveAll(s => s.Id == share) > 0;
        if (had) Emit(new { t = "done", share, outcome = "cancelled" });
    }

    public void SetVisible(bool everyone) { }

    public void Configure(string deviceName, string downloadDir) => _dir = downloadDir;

    // Trust, mocked: Ctrl+5 sends from "Asha's Galaxy" (a Drift device); once trusted, it skips the prompt.
    private readonly List<(string id, string name)> _trusted = new();
    private readonly Dictionary<ulong, string> _driftPeers = new();
    private void EmitTrusted() => Emit(new { t = "trusted", devices = _trusted.Select(d => new { d.id, d.name, auto = true }).ToArray() });
    public void Trust(ulong share) { lock (_lock) if (_driftPeers.TryGetValue(share, out var n) && !_trusted.Any(d => d.name == n)) _trusted.Add(("a3f09c21d4e7", n)); EmitTrusted(); }
    public void Untrust(string id) { lock (_lock) _trusted.RemoveAll(d => d.id == id); EmitTrusted(); }

    public void SimulateDrift()
    {
        var id = Interlocked.Increment(ref _next) - 1;
        var files = new[] { new FileInfo("Notes from Asha.pdf", 4_200_000) };
        bool trusted;
        lock (_lock) { _driftPeers[id] = "Asha's Galaxy"; trusted = _trusted.Any(d => d.name == "Asha's Galaxy"); _sims.Add(new Sim { Id = id, Total = files[0].Size, SavePath = Path.Combine(_dir, files[0].Name), Running = trusted }); }
        Emit(new { t = "incoming", share = id, from = "Asha's Galaxy", kind = "phone", pin = Pin(), files, direction = "incoming", drift = true, trusted, auto = trusted });
        if (trusted) Emit(new { t = "accepted", share = id, direction = "incoming" });
    }

    public void Dispose() => _stop.Cancel();
}

internal static class CtsExt
{
    public static bool IsCancelled(this CancellationTokenSource c) => c.IsCancellationRequested;
}
