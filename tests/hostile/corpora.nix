# Every hostile corpus, fetched by hash at test time and never committed (.claude/rules/002-dev.md). Each pin
# names its licence; PortSwigger's data and the MIME torture test have none.
{
  fetchFromGitHub,
  fetchFromGitLab,
  fetchgit,
  fetchurl,
}: {
  # BSD-2-Clause: every element and attribute that can load a URL, one leaking.via path each.
  httpleaks = fetchFromGitHub {
    owner = "cure53";
    repo = "HTTPLeaks";
    rev = "21b147cddb6f00c3178a2e191c28c97a3475afb3";
    hash = "sha256-ozNtfL9Jz9Y5Bim/6gWGhlwIpiSei2diVXR8R4gFJeM=";
  };
  # GPL-3.0+: emailprivacytester.com's tests, a snippet each with TRACKING_URL in place of the host.
  ept3 = fetchFromGitLab {
    owner = "grepular";
    repo = "ept3";
    rev = "b1b41fad6d6661bb7235b71914546dbeb072bbd7";
    hash = "sha256-34f3MWtqNFK/IjANvZS5R/+2p1BmNsRhchdINL3WdHo=";
  };
  # Apache-2.0 OR MPL-2.0: test/fixtures/expect.mjs, mXSS and namespace-confusion payloads.
  dompurify = fetchFromGitHub {
    owner = "cure53";
    repo = "DOMPurify";
    rev = "cde95990b2190338cfd7c00b768f3d81d3219d61";
    hash = "sha256-Cln3daj1fSuzapPZet3eIwlGg5zlYz0zXhxAC6ewDqE=";
  };
  # MPL-2.0: html5sec.org, items.js with %templates% from payloads.js.
  h5sc = fetchFromGitHub {
    owner = "cure53";
    repo = "H5SC";
    rev = "1b796a63b5ad8ca4734c52ed9cbe1035eb6f494b";
    hash = "sha256-iujfN6OYUBNVikyPtHUSsHgmSu6fedDxeaCBzH4RjaM=";
  };
  # No licence ("we are not providing a license"). Every event handler per tag, and more.
  portswigger = fetchFromGitHub {
    owner = "PortSwigger";
    repo = "xss-cheatsheet-data";
    rev = "bf194a0865465a3a4f71ff0f92bdd02fa659266c";
    hash = "sha256-ErzlzCGPo3gqgE3faPM1SS9rfHE4S/xdRP2V7MFrLyE=";
  };
  # BSD-3-Clause: the HTML parser's tree-construction tests and the Sanitizer API's safety cases.
  wpt = fetchgit {
    url = "https://github.com/web-platform-tests/wpt";
    rev = "b0912c91005c07733ba19a2d93c712e8547df31a";
    sparseCheckout = ["html/syntax/parsing/resources" "sanitizer-api"];
    hash = "sha256-5/eZ5LhwqG1Dc5SG+Cm3eWb3Z9C4duG9IoP5SHRyio4=";
  };
  # GPL-3.0-or-later: Roundcube 1.6.20's own XSS tests, where each CVE fix added its payload.
  roundcube = fetchgit {
    url = "https://github.com/roundcube/roundcubemail";
    rev = "1dd910522e73318447ed6e20bed1efd7adeb240a";
    sparseCheckout = ["tests"];
    hash = "sha256-5ik0ypkAbjR0B0MyTLAMPZ8j9Kr5lEqjQQd2bkL/Hbc=";
  };
  # Apache-2.0 OR MIT (thirdparty/: Dovecot's test mail, MIT): malformed MIME for the parser kubicle uses.
  mail-parser = fetchFromGitHub {
    owner = "stalwartlabs";
    repo = "mail-parser";
    rev = "acba893086305d5dff4ef568860495a56826f9f0";
    hash = "sha256-nAHuEdXE+y60dBBTfEB/61VbIoD+Npr0AIoqaMMVFk4=";
  };
  # MIT: bounces, digests, nested and broken multiparts.
  mimekit = fetchgit {
    url = "https://github.com/jstedfast/MimeKit";
    rev = "ea9d7eb5d5377d3c29f43d9112092f9aab5b7a31";
    sparseCheckout = ["UnitTests/TestData/messages"];
    hash = "sha256-wCKis9j4iBvDCuhBUEUbdfdM1ttcRd7UxELZKS5aHjw=";
  };
  # No stated licence (Mark Crispin, 1993). One message, 53 boundaries, deep nesting.
  mime-torture = fetchurl {
    url = "https://raw.githubusercontent.com/mozilla/releases-comm-central/c41443562054392ac017af5fb1ef73dc1f7df837/mailnews/test/data/mime-torture";
    hash = "sha256-EJDwBe6lArbR1+PEHRkLHH654O/9HRlRyi5L+wtNvPA=";
  };
  # MIT: sender-spoofing header shapes.
  espoofer = fetchurl {
    url = "https://raw.githubusercontent.com/chenjj/espoofer/60e5ed00f352999abb485bf12e8fdb2d83ddebf0/testcases.py";
    hash = "sha256-r8z9748tYF38e1btegeqVdZXBpXSuxe+nJ3exLCAGRA=";
  };
  # MIT: the Big List of Naughty Strings.
  blns = fetchurl {
    url = "https://raw.githubusercontent.com/minimaxir/big-list-of-naughty-strings/db33ec7b1d5d9616a88c76394b7d0897bd0b97eb/blns.json";
    hash = "sha256-te203/sjT6izfGNT7Cy9QUznIaA5aNJjQ6fCdqs2D2M=";
  };
}
