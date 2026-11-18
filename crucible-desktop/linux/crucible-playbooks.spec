# Packages binaries built beforehand (linux-bundle.sh puts them in SOURCES).
%global debug_package %{nil}

Name:           crucible-playbooks
Version:        %{version}
Release:        1%{?dist}
Summary:        Desktop tracker for Crucible playbooks
License:        MIT OR Apache-2.0
URL:            https://github.com/neuralmagic/crucible
Requires:       vulkan-loader
Recommends:     mesa-vulkan-drivers

%description
Crucible Playbooks shows what each Crucible playbook did and is doing, read from a Crucible
controller or local packs.

%install
install -Dm755 %{_sourcedir}/crucible-playbooks %{buildroot}%{_bindir}/crucible-playbooks
install -Dm755 %{_sourcedir}/crucible-lsp %{buildroot}%{_bindir}/crucible-lsp
install -Dm644 %{_sourcedir}/dev.crucible.playbooks.desktop %{buildroot}%{_datadir}/applications/dev.crucible.playbooks.desktop
install -Dm644 %{_sourcedir}/icon.png %{buildroot}%{_datadir}/icons/hicolor/512x512/apps/dev.crucible.playbooks.png

%files
%{_bindir}/crucible-playbooks
%{_bindir}/crucible-lsp
%{_datadir}/applications/dev.crucible.playbooks.desktop
%{_datadir}/icons/hicolor/512x512/apps/dev.crucible.playbooks.png
