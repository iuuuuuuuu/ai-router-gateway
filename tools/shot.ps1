
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Drawing;
public class WP {
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out R r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  public struct R { public int L, T, Rt, B; }
}
"@
$p = Get-Process gw-client -ErrorAction Stop | Select-Object -First 1
$h = $p.MainWindowHandle
Write-Output ("hwnd=" + $h + " visible=" + [WP]::IsWindowVisible($h))
$r = New-Object WP+R
[void][WP]::GetWindowRect($h, [ref]$r)
$w = $r.Rt - $r.L; $ht = $r.B - $r.T
Write-Output ("rect=" + $w + "x" + $ht)
$bmp = New-Object System.Drawing.Bitmap($w, $ht)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
$ok = [WP]::PrintWindow($h, $hdc, 2)
$g.ReleaseHdc($hdc)
$g.Dispose()
$out = Join-Path $PSScriptRoot "client-shot.png"
$bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Output ("printwindow=" + $ok + " saved=" + $out)
