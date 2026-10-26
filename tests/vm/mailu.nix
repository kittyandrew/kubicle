# The VM checks' machine (docs/design.md, Testing): a simplified copy of the owner's Mailu (kittyos
# selfhosted/mailu/default.nix) with kubicle's image in the webmail slot, so sign-in goes through front's
# auth_request and the session token, and mail through front's IMAP and submission proxies. The vm-* checks
# build on it: `node` is the machine, `setup` defines tier_setup(), which seeds it and readies the browser.
#
# Left out, as nothing kubicle does reaches them: rspamd and oletools (Postfix's milter is switched off instead),
# the unbound resolver (Docker's own DNS, plus a stub for admin's start check), Caddy in front of front.
{
  pkgs,
  kubicle,
}: {
  mail, # a tarball from mailbox.nix
  memorySize,
  roundcube ? false, # Mailu's own Roundcube beside kubicle under /roundcube, for a second account (settings.nix)
}: let
  ca = pkgs.callPackage ./ca.nix {};
  # The client under another build name, as a deploy brings one: `deploy_reaches_open_page` swaps it in.
  nextWeb = pkgs.runCommand "kubicle-web-next" {} ''
    cp -r ${kubicle.web} $out && chmod -R u+w $out && cd $out && find . -name '*.br' -delete
    wasm=$(echo kubicle_web_bg.*.wasm) boot=$(echo boot.*.js)
    mv "$wasm" kubicle_web_bg.0123456789abcdef.wasm && mv "$boot" boot.0123456789abcdef.js
    sed -i "s/$wasm/kubicle_web_bg.0123456789abcdef.wasm/g" boot.0123456789abcdef.js index.html
    sed -i "s/$boot/boot.0123456789abcdef.js/" index.html
  '';
  # Mailu 2024.06.64 by the digests the owner pins: Mailu rebuilds a tag in place.
  mailuImage = name: {
    digest,
    hash,
  }:
    pkgs.dockerTools.pullImage {
      imageName = "ghcr.io/mailu/${name}";
      imageDigest = "sha256:${digest}";
      inherit hash;
      finalImageName = "ghcr.io/mailu/${name}";
      finalImageTag = "2024.06.64";
    };
  images = {
    nginx = mailuImage "nginx" {
      digest = "64182efa7f4b6736636f4a89e2847d4d0cf3e7bb2acb8dc3fa4bef71a90cd624";
      hash = "sha256-CmQCLTeOiY1bQKCM37LIvdJMegPJ4K2gUNiANrH0gso=";
    };
    admin = mailuImage "admin" {
      digest = "7307e9bc8e5b027a8cc33b7d7801bece3a5676073d3bb46a64d23127e8f3ad2b";
      hash = "sha256-YvwOfDKDbyBnftpudLX7jdrbM2NPVWvshoz9wibX5gk=";
    };
    dovecot = mailuImage "dovecot" {
      digest = "39982c825630f6f93d120fa6164ed968eca96abe8300270ed37cbb28d969b909";
      hash = "sha256-9NSLtPwBkbUfXrHyiCoWjevPUkL3RcCF72img6fp5bo=";
    };
    postfix = mailuImage "postfix" {
      digest = "a97aefee1de11fd2bf1e19b35cfcf18ee16eb97dd2cce082ccde20eec374ca50";
      hash = "sha256-/uMllqlJHw89SZbQIut1GZBESAzXGhz1CsP8Ku1geWA=";
    };
    webmail = mailuImage "webmail" {
      digest = "e21bc80e84bc5adce513863e82e452733d7a9150188894aaa4c7a79bcb1a2bdb";
      hash = "sha256-Q/HxEFLqgcQOLxijnInbEmvN8nZlm2ueFdpq0tRMQ6s=";
    };
    redis = pkgs.dockerTools.pullImage {
      imageName = "docker.io/library/redis";
      imageDigest = "sha256:3811787313eba226a2ef38658c6ccb91cd5e110edc89c37767de373120a0e5a0";
      hash = "sha256-J2+OxrRkg+q+AxHlUgG3QWeR+mtm5EOGZFYpe9PPHnY=";
      finalImageName = "docker.io/library/redis";
      finalImageTag = "8.10.2-alpine";
    };
  };

  subnet = "192.168.203"; # the owner's Mailu subnet; .1 is the host
  webmailSubnet = "192.168.204";
  mailuEnv =
    {
      SUBNET = "${subnet}.0/24";
      DOMAIN = "example.com";
      HOSTNAMES = "mail.example.test";
      POSTMASTER = "admin";
      SECRET_KEY = "kubicle-vm-test-secret-key"; # a test value; the owner's lives in an agenix file
      TLS_FLAVOR = "mail";
      TLS_CERT_FILENAME = "server.pem";
      TLS_KEYPAIR_FILENAME = "server.key";
      DISABLE_STATISTICS = "True";
      ADMIN = "true";
      WEBMAIL = "roundcube"; # front creates the webmail locations only when this is not none
      API = "false";
      WEBDAV = "none";
      MESSAGE_SIZE_LIMIT = "50000000";
      RECIPIENT_DELIMITER = "+";
      WEBROOT_REDIRECT = "/login";
      WEB_ADMIN = "/admin";
      WEB_WEBMAIL = "/login";
      LOG_LEVEL = "WARNING";
    }
    # Mailu holds an account to 200 messages a day (MESSAGE_RATELIMIT), and vm-settings sends more.
    // pkgs.lib.optionalAttrs roundcube {MESSAGE_RATELIMIT_EXEMPTION = builtins.concatStringsSep "," users;};
  mailu = image: extra:
    {
      image = "ghcr.io/mailu/${image}:2024.06.64";
      imageFile = images.${image};
      environment = mailuEnv;
    }
    // extra;
  # The owner's override adds Archive; Mailu ships the other special-use folders.
  dovecotOverrides = pkgs.writeTextDir "dovecot.conf" ''
    namespace inbox {
      mailbox Archive {
        auto = subscribe
        special_use = \Archive
      }
    }
  '';
  # No rspamd here, and Mailu's Postfix tempfails every mail its milter cannot reach (milter_default_action).
  postfixOverrides = pkgs.writeTextDir "postfix.cf" "smtpd_milters =\n";
  # Every account seeded with the same mailbox; Roundcube's is the second.
  users = ["andrew@example.com"] ++ pkgs.lib.optional roundcube "roundcube@example.com";
  accounts = (pkgs.formats.yaml {}).generate "accounts.yml" {
    domain = [{name = "example.com";}];
    user =
      map (email: {
        inherit email;
        password = "fixture-password";
        hash_password = true;
        change_pw_next_login = false;
      })
      users;
  };
  # Roundcube's webmail location beside kubicle's, as front's nginx.conf writes its own: auth_request on every
  # path, and Mailu's identity and token handed over on sso.php alone. `set` comes first, as `rewrite ... break`
  # ends the rewrite module's directives.
  frontOverrides = pkgs.writeTextDir "roundcube.conf" ''
    location /roundcube {
      set $roundcube roundcube;
      rewrite ^(/roundcube)$ $1/ permanent;
      rewrite ^/roundcube/(.*) /$1 break;
      include /etc/nginx/proxy.conf;
      auth_request /internal/auth/user;
      error_page 403 @sso_login;
      proxy_pass http://$roundcube;
    }
    location /roundcube/sso.php {
      set $roundcube roundcube;
      rewrite ^/roundcube/(.*) /$1 break;
      include /etc/nginx/proxy.conf;
      auth_request /internal/auth/user;
      auth_request_set $user $upstream_http_x_user;
      auth_request_set $token $upstream_http_x_user_token;
      proxy_set_header X-Remote-User $user;
      proxy_set_header X-Remote-User-Token $token;
      error_page 403 @sso_login;
      proxy_pass http://$roundcube;
    }
  '';
  # A second mail client, as a phone app connects: IMAPS on front's public port with the account password.
  imapClient = pkgs.writers.writePython3Bin "imap-client" {flakeIgnore = ["E501"];} ''
    """usage: imap-client FOLDER flag UID FLAG | move UID TO | expunge UID"""
    import imaplib
    import ssl
    import sys

    folder, command, uid, *rest = sys.argv[1:]
    context = ssl.create_default_context(cafile="${ca}/ca.pem")
    with imaplib.IMAP4_SSL("mail.example.test", ssl_context=context) as imap:
        imap.login("andrew@example.com", "fixture-password")
        imap.select(folder)
        if command == "flag":
            imap.uid("STORE", uid, "+FLAGS", rest[0])
        elif command == "move":
            imap.uid("MOVE", uid, rest[0])
        elif command == "expunge":
            imap.uid("STORE", uid, "+FLAGS", "\\Deleted")
            imap.uid("EXPUNGE", uid)
  '';
  # Mailu admin's start loops until a resolver answers example.org with the AD bit (core/admin/start.py,
  # test_DNS). The VM has no Internet to validate against, so this stub answers every A question with it.
  dnsStub = pkgs.writers.writePython3 "dns-stub" {libraries = [pkgs.python3Packages.dnslib];} ''
    from dnslib import QTYPE, RR, A
    from dnslib.server import BaseResolver, DNSServer


    class Validated(BaseResolver):
        def resolve(self, request, handler):
            reply = request.reply()
            reply.header.ad = 1
            if request.q.qtype == QTYPE.A:
                reply.add_answer(RR(request.q.qname, rdata=A("192.0.2.1")))
            return reply


    DNSServer(Validated(), address="${subnet}.1", port=53).start()
  '';
in {
  node = {
    imports = [./browser.nix ../capture.nix];
    # With the balloon below, the guest hands freed memory back to the host; reporting every free page, not only
    # free 2 MB blocks, makes that cover what Chromium and the page cache let go: 490 MB host RSS after a run,
    # against 996 MB at the default order.
    boot.kernelParams = ["page_reporting.page_reporting_order=0"];
    virtualisation = {
      inherit memorySize;
      qemu.options = ["-device virtio-balloon-pci,free-page-reporting=on"];
      diskSize = 8192;
      cores = 4;
    };
    networking.firewall.enable = false; # containers reach the DNS stub on the bridge
    environment.systemPackages = [pkgs.swaks pkgs.curl pkgs.iptables pkgs.zstd imapClient];
    networking.extraHosts = "${subnet}.10 mail.example.test"; # front, for the second client's certificate check
    virtualisation.docker.enable = true;
    virtualisation.oci-containers.backend = "docker";

    systemd.tmpfiles.rules = [
      "d /var/lib/kubicle 0700 65534 65534 -" # the image runs as nobody
      "L+ /var/lib/kubicle/web - - - - ${kubicle.web}" # KUBICLE_WEB_DIR: a test repoints it to deploy `nextWeb`
    ];
    systemd.services.docker-mailu-roundcube = pkgs.lib.mkIf roundcube {
      after = ["docker-networks.service"];
      requires = ["docker-networks.service"];
    };
    systemd.services.docker-networks = {
      after = ["docker.service"];
      requires = ["docker.service"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      path = [pkgs.docker];
      script = ''
        docker network create --subnet=${subnet}.0/24 mailu
        docker network create --subnet=${webmailSubnet}.0/24 webmail
      '';
    };
    systemd.services.dns-stub = {
      after = ["docker-networks.service"];
      requires = ["docker-networks.service"];
      serviceConfig.ExecStart = dnsStub;
    };
    systemd.services.docker-mailu-redis = {
      after = ["docker-networks.service"];
      requires = ["docker-networks.service"];
    };
    systemd.services.docker-mailu-admin = {
      after = ["dns-stub.service"];
      requires = ["dns-stub.service"];
    };
    systemd.services.docker-mailu-imap = {
      after = ["docker-networks.service"];
      requires = ["docker-networks.service"];
    };
    systemd.services.docker-mailu-smtp = {
      after = ["docker-networks.service"];
      requires = ["docker-networks.service"];
    };

    # The compose names Mailu's configs hardcode (front, admin, imap, smtp, redis, webmail) as network aliases.
    virtualisation.oci-containers.containers = {
      mailu-redis = {
        image = "docker.io/library/redis:8.10.2-alpine";
        imageFile = images.redis;
        volumes = ["/var/lib/mailu/redis:/data"];
        networks = ["name=mailu,alias=redis"];
      };
      mailu-admin = mailu "admin" {
        dependsOn = ["mailu-redis"];
        volumes = ["/var/lib/mailu/data:/data" "/var/lib/mailu/dkim:/dkim"];
        networks = ["name=mailu,alias=admin"];
        extraOptions = ["--dns=${subnet}.1"];
      };
      mailu-imap = mailu "dovecot" {
        volumes = ["/var/lib/mailu/mail:/mail" "${dovecotOverrides}:/overrides:ro"];
        networks = ["name=mailu,alias=imap,ip=${subnet}.11"];
        extraOptions = ["--ulimit=nofile=25000:25000"]; # managesieve-login wants 25000 descriptors
      };
      mailu-smtp = mailu "postfix" {
        volumes = ["/var/lib/mailu/queue:/queue" "${postfixOverrides}:/overrides:ro"];
        networks = ["name=mailu,alias=smtp"];
      };
      mailu-front = mailu "nginx" {
        dependsOn = ["mailu-imap" "mailu-smtp" "mailu-admin"]; # nginx exits if an upstream does not resolve
        ports = ["80:80"];
        volumes = ["${ca}:/certs:ro"] ++ pkgs.lib.optional roundcube "${frontOverrides}:/overrides:ro";
        networks = ["name=mailu,alias=front,ip=${subnet}.10" "name=webmail,alias=front"];
      };
      kubicle = {
        image = "kubicle:latest";
        imageFile = kubicle.image;
        dependsOn = ["mailu-front"];
        volumes = ["/var/lib/kubicle:/var/lib/kubicle" "${ca}/ca.pem:/ca.pem:ro" "${nextWeb}:${nextWeb}:ro"];
        networks = ["name=webmail,alias=webmail"];
        environment = {
          KUBICLE_WEB_DIR = "/var/lib/kubicle/web";
          KUBICLE_PUBLIC_ORIGIN = "http://localhost";
          KUBICLE_MAIL_TLS_NAME = "mail.example.test";
          KUBICLE_MAIL_EXTRA_CA = "/ca.pem";
          KUBICLE_MAIL_DOMAINS = "example.com";
          KUBICLE_TIME_ZONE = "Europe/Kyiv";
          KUBICLE_TRUSTED_PROXY = "${webmailSubnet}.0/24";
          KUBICLE_AUTHSERV_ID = mailuEnv.HOSTNAMES; # Mailu's first HOSTNAMES entry, as the owner's sets it
          RUST_LOG = "info,kubicle_server=debug,kubicle_mailbox=debug,kubicle_imap=debug"; # a line per request (tests/record.py) and IMAP command, a stopped search
        };
      };
      mailu-roundcube = pkgs.lib.mkIf roundcube (mailu "webmail" {
        dependsOn = ["mailu-front"]; # its nginx exits if `front` does not resolve
        environment =
          mailuEnv
          // {
            WEB_WEBMAIL = "/roundcube"; # Roundcube's request_path, the prefix front strips
            HOSTNAMES = "localhost"; # Roundcube's trusted_host_patterns
            ADMIN = "false";
            TZ = "Europe/Kyiv"; # PHP's zone, Roundcube's "Auto" under sso.php: kubicle's KUBICLE_TIME_ZONE
          };
        volumes = ["/var/lib/roundcube:/data" "${./roundcube-prefs.php}:/kdt/roundcube-prefs.php:ro"];
        networks = ["name=webmail,alias=roundcube"];
      });
    };
  };
  setup = ''
    def tier_setup() -> None:
        # Mailu's sign-in form validates addresses with email_validator, which refuses the reserved .test domain
        # the fixture uses, so the account here is at example.com; seed.py's mail is the same.
        tier.user = "andrew@example.com"
        tier.next_web = "${nextWeb}"
        status = "-s -o /dev/null -w '%{http_code}'"
        start_all()
        machine.wait_for_unit("docker-mailu-imap.service")
        machine.wait_until_succeeds("docker exec mailu-imap true", timeout=120)
        for user in ${builtins.toJSON users}:
            machine.succeed(f"mkdir -p /var/lib/mailu/mail/{user} && tar -C /var/lib/mailu/mail/{user} --zstd -xf ${mail}")
            machine.succeed(f"docker exec mailu-imap chown -R mail:mail /mail/{user}")
        machine.wait_until_succeeds("docker exec mailu-admin curl -sf http://127.0.0.1:8080/ping", timeout=300)
        machine.succeed("docker exec -i mailu-admin flask mailu config-import --update < ${accounts}")
        # The items a list page's FETCH asks for: Dovecot caches them only once a client asked, so a first
        # fetch of all of INBOX takes ~5 s.
        fields = "flags size.virtual date.received imap.envelope hdr.list-post hdr.content-type"
        machine.succeed(f"docker exec mailu-imap doveadm fetch -u {tier.user} '{fields}' mailbox INBOX all >/dev/null")
        machine.wait_until_succeeds("curl -sf http://localhost/health", timeout=120)
        machine.wait_for_unit("docker-kubicle.service")
        machine.wait_until_succeeds("docker exec mailu-front curl -sf http://webmail/healthz", timeout=60)

        machine.wait_for_unit("chromedriver.service")
        machine.wait_for_open_port(9515)
        machine.succeed("wd start")

        def mailu_sign_in() -> None:
            wd("go", "http://localhost/login/")
            wait("document.querySelector('input[name=pw]')")
            wd("type", "input[name=email]", tier.user)
            wd("type", "input[name=pw]", "fixture-password")
            wd("click", "form [type=submit]")

        tier.sign_in = mailu_sign_in
        tier.deliver = lambda subject, headers="": (
            f"swaks --silent 2 --protocol LMTP --server ${subnet}.11:2525 --to {tier.user}"
            + f" --from robot@example.org {headers} --h-Subject {shlex.quote(subject)}"
        )
        tier.doveadm = "docker exec mailu-imap doveadm"
        tier.size_limit = ${mailuEnv.MESSAGE_SIZE_LIMIT}  # what front's EHLO says as SIZE
        tier.forged_sso = (
            f"docker exec mailu-front curl {status} -H 'X-Remote-User: {tier.user}'"
            + " -H 'X-Remote-User-Token: token-forged' http://webmail/sso.php"
        )
  '';
}
