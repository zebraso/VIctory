Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
"@
$dir = 'C:\OpenCodeProjects\VIctory-vit\capsec'
New-Item -ItemType Directory -Force $dir | Out-Null
Get-ChildItem $dir -File -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
$last = [IntPtr]::Zero
for ($i = 0; $i -lt 200; $i++) {
    $p = Get-Process victory -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
    if ($null -eq $p) { Start-Sleep -Seconds 5; continue }
    $h = $p.MainWindowHandle
    if ($h -ne $last) { [Win]::ShowWindow($h, 9) | Out-Null; [Win]::SetForegroundWindow($h) | Out-Null; $last = $h }
    $r = New-Object Win+RECT
    if ([Win]::GetWindowRect($h, [ref]$r)) {
        $w = $r.Right - $r.Left; $ht = $r.Bottom - $r.Top
        if ($w -gt 64 -and $ht -gt 64) {
            try {
                $bmp = New-Object System.Drawing.Bitmap $w, $ht
                $g = [System.Drawing.Graphics]::FromImage($bmp)
                $g.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size $w, $ht))
                $bmp.Save("$dir\shot-$i.png", [System.Drawing.Imaging.ImageFormat]::Png)
                $g.Dispose(); $bmp.Dispose()
            } catch { }
        }
    }
    Start-Sleep -Seconds 5
}
