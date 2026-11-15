using System.Runtime.InteropServices;

namespace Drift.Shell;

/// Drift's notification-area icon (Win32 Shell_NotifyIcon on the main window's HWND, no extra
/// dependency). Click: open the window. Right-click: Open / Quit. Survives Explorer restarts.
public sealed class Tray : IDisposable
{
    private const uint WM_TRAY = 0x8000 + 1; // WM_APP + 1
    private const uint NIM_ADD = 0, NIM_MODIFY = 1, NIM_DELETE = 2, NIM_SETVERSION = 4;
    private const uint NIF_MESSAGE = 1, NIF_ICON = 2, NIF_TIP = 4, NIF_SHOWTIP = 0x80;
    private const uint WM_CONTEXTMENU = 0x007B, WM_LBUTTONUP = 0x0202, WM_RBUTTONUP = 0x0205, NIN_SELECT = 0x0400, NIN_KEYSELECT = 0x0401;
    private const uint MF_STRING = 0, MF_SEPARATOR = 0x800, TPM_RETURNCMD = 0x100, TPM_RIGHTBUTTON = 2, TPM_BOTTOMALIGN = 0x20;
    private const uint IMAGE_ICON = 1, LR_LOADFROMFILE = 0x10;

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct NOTIFYICONDATAW
    {
        public uint cbSize;
        public nint hWnd;
        public uint uID;
        public uint uFlags;
        public uint uCallbackMessage;
        public nint hIcon;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string szTip;
        public uint dwState, dwStateMask;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)] public string szInfo;
        public uint uVersion;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 64)] public string szInfoTitle;
        public uint dwInfoFlags;
        public Guid guidItem;
        public nint hBalloonIcon;
    }

    [StructLayout(LayoutKind.Sequential)] private struct POINT { public int X, Y; }
    private delegate nint SubclassProc(nint hWnd, uint msg, nint wParam, nint lParam, nuint id, nuint refData);

    [DllImport("shell32.dll", CharSet = CharSet.Unicode)] private static extern bool Shell_NotifyIconW(uint msg, ref NOTIFYICONDATAW data);
    [DllImport("comctl32.dll")] private static extern bool SetWindowSubclass(nint hWnd, SubclassProc proc, nuint id, nuint refData);
    [DllImport("comctl32.dll")] private static extern bool RemoveWindowSubclass(nint hWnd, SubclassProc proc, nuint id);
    [DllImport("comctl32.dll")] private static extern nint DefSubclassProc(nint hWnd, uint msg, nint wParam, nint lParam);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] private static extern uint RegisterWindowMessageW(string name);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] private static extern nint LoadImageW(nint hInst, string name, uint type, int cx, int cy, uint flags);
    [DllImport("user32.dll")] private static extern bool DestroyIcon(nint icon);
    [DllImport("user32.dll")] private static extern int GetSystemMetricsForDpi(int index, uint dpi);
    [DllImport("user32.dll")] private static extern uint GetDpiForWindow(nint hWnd);
    [DllImport("user32.dll")] private static extern nint CreatePopupMenu();
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] private static extern bool AppendMenuW(nint menu, uint flags, nuint id, string? text);
    [DllImport("user32.dll")] private static extern uint TrackPopupMenu(nint menu, uint flags, int x, int y, int reserved, nint hWnd, nint rect);
    [DllImport("user32.dll")] private static extern bool DestroyMenu(nint menu);
    [DllImport("user32.dll")] private static extern bool GetCursorPos(out POINT p);
    [DllImport("user32.dll")] private static extern bool SetForegroundWindow(nint hWnd);

    private readonly nint _hwnd;
    private readonly SubclassProc _proc; // kept alive for the native side
    private readonly uint _taskbarCreated;
    private readonly Action _open, _quit;
    private nint _icon;
    private bool _added;

    public Tray(nint hwnd, string iconPath, Action open, Action quit)
    {
        _hwnd = hwnd; _open = open; _quit = quit;
        var dpi = GetDpiForWindow(hwnd);
        var size = GetSystemMetricsForDpi(49 /* SM_CXSMICON */, dpi);
        _icon = LoadImageW(0, iconPath, IMAGE_ICON, size, size, LR_LOADFROMFILE);
        _taskbarCreated = RegisterWindowMessageW("TaskbarCreated");
        _proc = Proc;
        SetWindowSubclass(hwnd, _proc, 1, 0);
        Add();
    }

    private NOTIFYICONDATAW Data(uint flags) => new()
    {
        cbSize = (uint)Marshal.SizeOf<NOTIFYICONDATAW>(),
        hWnd = _hwnd, uID = 1, uFlags = flags, uCallbackMessage = WM_TRAY, hIcon = _icon,
        szTip = "Drift", szInfo = "", szInfoTitle = "", uVersion = 4,
    };

    private void Add()
    {
        var d = Data(NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP);
        _added = Shell_NotifyIconW(NIM_ADD, ref d);
        Shell_NotifyIconW(NIM_SETVERSION, ref d);
    }

    /// Tooltip line, e.g. "Drift · visible 9:40" or "Drift · receiving 42%".
    public void SetTip(string tip)
    {
        if (!_added) return;
        var d = Data(NIF_TIP | NIF_SHOWTIP);
        d.szTip = tip.Length > 120 ? tip[..120] : tip;
        Shell_NotifyIconW(NIM_MODIFY, ref d);
    }

    private nint Proc(nint hWnd, uint msg, nint wParam, nint lParam, nuint id, nuint refData)
    {
        if (msg == WM_TRAY)
        {
            switch ((uint)(lParam & 0xFFFF))
            {
                case WM_LBUTTONUP or NIN_SELECT or NIN_KEYSELECT: _open(); break;
                case WM_CONTEXTMENU or WM_RBUTTONUP: Menu(); break;
            }
            return 0;
        }
        if (msg == _taskbarCreated) Add(); // Explorer restarted: put the icon back
        return DefSubclassProc(hWnd, msg, wParam, lParam);
    }

    private void Menu()
    {
        var m = CreatePopupMenu();
        AppendMenuW(m, MF_STRING, 1, "Open Drift");
        AppendMenuW(m, MF_SEPARATOR, 0, null);
        AppendMenuW(m, MF_STRING, 2, "Quit Drift");
        GetCursorPos(out var p);
        SetForegroundWindow(_hwnd); // so the menu closes when you click elsewhere
        var cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, p.X, p.Y, 0, _hwnd, 0);
        DestroyMenu(m);
        if (cmd == 1) _open();
        else if (cmd == 2) _quit();
    }

    public void Dispose()
    {
        if (_added) { var d = Data(0); Shell_NotifyIconW(NIM_DELETE, ref d); _added = false; }
        RemoveWindowSubclass(_hwnd, _proc, 1);
        if (_icon != 0) { DestroyIcon(_icon); _icon = 0; }
    }
}
