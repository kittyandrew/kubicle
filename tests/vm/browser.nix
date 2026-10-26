# Headless Chromium for the VM tests, behind chromedriver, and `wd` (wd.sh) to drive it from the test script.
{pkgs, ...}: let
  wd = pkgs.writeShellApplication {
    name = "wd";
    runtimeInputs = [pkgs.curl pkgs.jq pkgs.uutils-coreutils-noprefix];
    runtimeEnv.CHROMIUM = pkgs.lib.getExe pkgs.chromium;
    text = builtins.readFile ./wd.sh;
  };
in {
  environment.systemPackages = [wd];
  fonts.enableDefaultPackages = true; # else every glyph is a fallback box and text measures differently

  systemd.services.chromedriver = {
    wantedBy = ["multi-user.target"];
    serviceConfig.ExecStart = "${pkgs.lib.getExe pkgs.chromedriver} --port=9515";
  };
}
