# The exploration suite (docs/design.md, Testing): Bombadil drives the real client in Chromium at random against
# the fixture mailbox and checks spec.ts's properties in every state; test_trace.py then pins what it saw. The run
# records its trace whatever it found (../record.py), so a failing pin can be fixed from that output with
# `--inline-snapshot=fix`; the check fails on a violation, a leaked packet or a changed pin.
{
  pkgs,
  server,
  web,
}: let
  judge = import ../judge.nix {inherit pkgs;};
  seconds = 300; # each walk; test_trace.py fails one that ends before 80% of it
  bombadil = pkgs.callPackage ./bombadil.nix {};
in
  judge (pkgs.testers.runNixOSTest {
    name = "kubicle-explore";
    extraPythonPackages = p: [p.pytest p.inline-snapshot]; # test_trace.py's
    nodes.explorer = {
      imports = [
        (import ../stack.nix {
          inherit web;
          browser = pkgs.chromium;
          sessions = [server];
          seed = "mkdir -p $MAIL && tar -C $MAIL --zstd -xf ${pkgs.callPackage ../vm/mailbox.nix {}}/full.tar.zst";
        })
      ];
      virtualisation = {
        memorySize = 2048; # 1.1 GB used at most (free -m) while Bombadil explores
        cores = 4;
        diskSize = 4096;
      };
    };
    testScript =
      builtins.readFile ../record.py
      + ''
        explorer.wait_for_unit("kubicle1.service")
        explorer.wait_for_unit("caddy.service")
        explorer.wait_for_open_port(8088)
        # Bombadil exits non-zero on a violation; test_trace.py reports it from the trace.
        explorer.execute(
            "cd /var/lib/browser1 && CHROME=/run/current-system/sw/bin/browser1 ${bombadil}/bin/bombadil browser test "
            + "--headless --time-limit ${toString seconds}s --output-path explore "
            + "http://localhost:8088/login/sso.php ${./spec.ts} >&2",
            timeout=900,
        )
        explorer.succeed("dmesg | grep 'kubicle-leak1 ' > /var/lib/browser1/leaks.log || true; cp /var/log/sink1.log /var/lib/browser1/sink.log")
        for path in ["explore", "leaks.log", "sink.log"]:
            explorer.copy_from_machine(f"/var/lib/browser1/{path}")
        collect(explorer, "/var/lib/browser1/netlog-*.json", "/var/log/caddy")  # the journal too
        import contextlib, pytest, shutil
        os.environ["KUBICLE_EXPLORE"] = os.environ["out"]
        os.environ["KUBICLE_EXPLORE_SECONDS"] = "${toString seconds}"
        # A copy, so pytest imports it as test_trace from a writable directory, not as a store path's module; run
        # from there, as inline-snapshot's report fails on a test file outside the working directory.
        trace = shutil.copyfile("${./test_trace.py}", os.path.join(os.environ["TMPDIR"], "test_trace.py"))
        with contextlib.chdir(os.environ["TMPDIR"]):
            failed = pytest.main(["-p", "no:cacheprovider", trace]) != 0
        if failed:
            fail("test_trace.py: its failures are in the run's log")
        record()
      '';
  })
