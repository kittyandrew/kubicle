# vm-full's remote image host for `image_proxy` (scenarios.py): images.example.net, a public-looking address that
# only kubicle's container resolves, in a network namespace of its own, as front's published port 80 holds the
# host's.
{pkgs, ...}: let
  address = "11.0.0.80";
  # Logs every request's path and headers as a JSON line, for the scenario to read. `/slow*.png` takes 1.5 s and logs
  # how many requests are in flight, for `image_proxy_bounded`.
  server = pkgs.writers.writePython3 "image-server" {flakeIgnore = ["E501"];} ''
    import base64
    import json
    import threading
    import time
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    PNG = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
    REPLIES = {"/pixel.png": (200, "image/png", PNG), "/page.png": (200, "text/html", b"<p>Not an image.</p>")}
    INFLIGHT, LOCK = [0], threading.Lock()


    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path.startswith("/slow"):
                with LOCK:
                    INFLIGHT[0] += 1
                    print(json.dumps({"path": self.path, "inflight": INFLIGHT[0]}), flush=True)
                time.sleep(1.5)
                with LOCK:
                    INFLIGHT[0] -= 1
                self.send_response(200)
                self.send_header("Content-Type", "image/png")
                self.send_header("Content-Length", str(len(PNG)))
                self.end_headers()
                self.wfile.write(PNG)
                return
            print(json.dumps({"path": self.path, "headers": dict(self.headers)}), flush=True)
            if self.path == "/moved.png":
                self.send_response(302)
                self.send_header("Location", "/pixel.png")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            status, kind, body = REPLIES.get(self.path, (404, "text/plain", b""))
            self.send_response(status)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)


    ThreadingHTTPServer(("${address}", 80), Handler).serve_forever()
  '';
  inNamespace = {
    wantedBy = ["multi-user.target"];
    after = ["image-namespace.service"];
    requires = ["image-namespace.service"];
    serviceConfig.NetworkNamespacePath = "/run/netns/images";
  };
in {
  systemd.services.image-namespace = {
    wantedBy = ["multi-user.target"];
    path = [pkgs.iproute2];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    script = ''
      ip netns add images
      ip link add images0 type veth peer name images1 netns images
      ip addr add 11.0.0.1/24 dev images0 && ip link set images0 up
      ip -n images addr add ${address}/24 dev images1 && ip -n images link set images1 up && ip -n images link set lo up
      ip -n images route add default via 11.0.0.1
    '';
  };
  systemd.services.image-dns = pkgs.lib.recursiveUpdate inNamespace {
    serviceConfig.ExecStart = "${pkgs.dnsmasq}/bin/dnsmasq --keep-in-foreground --pid-file= --no-resolv --no-hosts --bind-interfaces --listen-address=${address} --address=/images.example.net/${address}";
  };
  systemd.services.image-server = pkgs.lib.recursiveUpdate inNamespace {
    serviceConfig = {
      ExecStart = server;
      StandardOutput = "append:/var/log/image-server.log";
      StandardError = "journal"; # http.server's access lines, else they land in the JSON log above
    };
  };
  # Docker's own DNS answers the containers' names and asks this one for the rest.
  virtualisation.oci-containers.containers.kubicle.extraOptions = ["--dns=${address}"];
}
