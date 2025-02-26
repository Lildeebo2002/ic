"""
A macro to build multiple versions of the ICOS image (i.e., dev vs prod)
"""

load("@bazel_skylib//rules:copy_file.bzl", "copy_file")
load("//bazel:defs.bzl", "gzip_compress", "sha256sum2url", "zstd_compress")
load("//bazel:output_files.bzl", "output_files")
load("//gitlab-ci/src/artifacts:upload.bzl", "upload_artifacts")
load("//ic-os/bootloader:defs.bzl", "build_grub_partition")
load("//toolchains/sysimage:toolchain.bzl", "build_container_filesystem", "disk_image", "ext4_image", "sha256sum", "tar_extract", "upgrade_image")

def icos_build(
        name,
        upload_prefix,
        image_deps_func,
        mode = None,
        malicious = False,
        upgrades = True,
        vuln_scan = True,
        visibility = None,
        ic_version = "//bazel:version.txt"):
    """
    Generic ICOS build tooling.

    Args:
      name: Name for the generated filegroup.
      upload_prefix: Prefix to be used as the target when uploading
      image_deps_func: Function to be used to generate image manifest
      mode: dev or prod. If not specified, will use the value of `name`
      malicious: if True, bundle the `malicious_replica`
      upgrades: if True, build upgrade images as well
      vuln_scan: if True, create targets for vulnerability scanning
      visibility: See Bazel documentation
      ic_version: the label pointing to the target that returns IC version
    """

    if mode == None:
        mode = name

    image_deps = image_deps_func(mode, malicious)

    # -------------------- Version management --------------------

    copy_file(
        name = "copy_version_txt",
        src = ic_version,
        out = "version.txt",
        allow_symlink = True,
        visibility = visibility,
        tags = ["manual"],
    )

    if upgrades:
        native.genrule(
            name = "test_version_txt",
            srcs = [":copy_version_txt"],
            outs = ["version-test.txt"],
            cmd = "sed -e 's/.*/&-test/' < $< > $@",
            tags = ["manual"],
        )

    # -------------------- Build grub partition --------------------

    build_grub_partition("partition-grub.tar", grub_config = image_deps.get("grub_config", default = None), tags = ["manual"])

    # -------------------- Build the container image --------------------

    build_container_filesystem_config_file = Label(image_deps.get("build_container_filesystem_config_file"))

    build_container_filesystem(
        name = "rootfs-tree.tar",
        context_files = [image_deps["container_context_files"]],
        config_file = build_container_filesystem_config_file,
        target_compatible_with = ["@platforms//os:linux"],
        tags = ["manual"],
    )

    tar_extract(
        name = "file_contexts",
        src = "rootfs-tree.tar",
        path = "etc/selinux/default/contexts/files/file_contexts",
        target_compatible_with = [
            "@platforms//os:linux",
        ],
        tags = ["manual"],
    )

    # -------------------- Extract root partition --------------------

    ext4_image(
        name = "partition-root-unsigned.tar",
        src = _dict_value_search(image_deps["rootfs"], "/"),
        # Take the dependency list declared above, and add in the "version.txt"
        # at the correct place.
        extra_files = {
            k: v
            for k, v in (image_deps["rootfs"].items() + [(":version.txt", "/opt/ic/share/version.txt:0644")])
            # Skip over special entries
            if v != "/"
        },
        file_contexts = ":file_contexts",
        partition_size = image_deps["rootfs_size"],
        strip_paths = [
            "/run",
            "/boot",
            "/var",
        ],
        target_compatible_with = [
            "@platforms//os:linux",
        ],
        tags = ["manual"],
    )

    if upgrades:
        ext4_image(
            name = "partition-root-test-unsigned.tar",
            src = _dict_value_search(image_deps["rootfs"], "/"),
            # Take the dependency list declared above, and add in the "version.txt"
            # at the correct place.
            extra_files = {
                k: v
                for k, v in (image_deps["rootfs"].items() + [(":version-test.txt", "/opt/ic/share/version.txt:0644")])
                # Skip over special entries
                if v != "/"
            },
            file_contexts = ":file_contexts",
            partition_size = image_deps["rootfs_size"],
            strip_paths = [
                "/run",
                "/boot",
            ],
            target_compatible_with = [
                "@platforms//os:linux",
            ],
            tags = ["manual"],
        )

    # -------------------- Extract boot partition --------------------

    if "boot_args_template" not in image_deps:
        native.alias(name = "partition-root.tar", actual = ":partition-root-unsigned.tar")
        native.alias(name = "extra_boot_args", actual = image_deps["extra_boot_args"])

        if upgrades:
            native.alias(name = "partition-root-test.tar", actual = ":partition-root-test-unsigned.tar")
            native.alias(name = "extra_boot_test_args", actual = image_deps["extra_boot_args"])
    else:
        native.alias(name = "extra_boot_args_template", actual = image_deps["boot_args_template"])

        native.genrule(
            name = "partition-root-sign",
            srcs = ["partition-root-unsigned.tar"],
            outs = ["partition-root.tar", "partition-root-hash"],
            cmd = "$(location //toolchains/sysimage:verity_sign.py) -i $< -o $(location :partition-root.tar) -r $(location partition-root-hash)",
            executable = False,
            tools = ["//toolchains/sysimage:verity_sign.py"],
            tags = ["manual"],
        )

        native.genrule(
            name = "extra_boot_args_root_hash",
            srcs = [
                ":extra_boot_args_template",
                ":partition-root-hash",
            ],
            outs = ["extra_boot_args"],
            cmd = "sed -e s/ROOT_HASH/$$(cat $(location :partition-root-hash))/ < $(location :extra_boot_args_template) > $@",
            tags = ["manual"],
        )

        if upgrades:
            native.genrule(
                name = "partition-root-test-sign",
                srcs = ["partition-root-test-unsigned.tar"],
                outs = ["partition-root-test.tar", "partition-root-test-hash"],
                cmd = "$(location //toolchains/sysimage:verity_sign.py) -i $< -o $(location :partition-root-test.tar) -r $(location partition-root-test-hash)",
                tools = ["//toolchains/sysimage:verity_sign.py"],
                tags = ["manual"],
            )

            native.genrule(
                name = "extra_boot_args_root_test_hash",
                srcs = [
                    ":extra_boot_args_template",
                    ":partition-root-test-hash",
                ],
                outs = ["extra_boot_test_args"],
                cmd = "sed -e s/ROOT_HASH/$$(cat $(location :partition-root-test-hash))/ < $(location :extra_boot_args_template) > $@",
                tags = ["manual"],
            )

    ext4_image(
        name = "partition-boot.tar",
        src = _dict_value_search(image_deps["bootfs"], "/"),
        # Take the dependency list declared above, and add in the "version.txt"
        # as well as the generated extra_boot_args file in the correct place.
        extra_files = {
            k: v
            for k, v in (
                image_deps["bootfs"].items() + [
                    (":version.txt", "/boot/version.txt:0644"),
                    (":extra_boot_args", "/boot/extra_boot_args:0644"),
                ]
            )
            # Skip over special entries
            if v != "/"
        },
        file_contexts = ":file_contexts",
        partition_size = image_deps["bootfs_size"],
        subdir = "boot/",
        target_compatible_with = [
            "@platforms//os:linux",
        ],
        tags = ["manual"],
    )

    if upgrades:
        ext4_image(
            name = "partition-boot-test.tar",
            src = _dict_value_search(image_deps["rootfs"], "/"),
            # Take the dependency list declared above, and add in the "version.txt"
            # as well as the generated extra_boot_args file in the correct place.
            extra_files = {
                k: v
                for k, v in (
                    image_deps["bootfs"].items() + [
                        (":version-test.txt", "/boot/version.txt:0644"),
                        (":extra_boot_test_args", "/boot/extra_boot_args:0644"),
                    ]
                )
                # Skip over special entries
                if v != "/"
            },
            file_contexts = ":file_contexts",
            partition_size = image_deps["bootfs_size"],
            subdir = "boot/",
            target_compatible_with = [
                "@platforms//os:linux",
            ],
            tags = ["manual"],
        )

    # -------------------- Assemble disk image --------------------

    # Build a list of custom partitions with a function, to allow "injecting" build steps at this point
    if "custom_partitions" not in image_deps:
        custom_partitions = []
    else:
        custom_partitions = image_deps["custom_partitions"]()

    disk_image(
        name = "disk-img.tar",
        layout = image_deps["partition_table"],
        partitions = [
            "//ic-os/bootloader:partition-esp.tar",
            ":partition-grub.tar",
            ":partition-boot.tar",
            ":partition-root.tar",
        ] + custom_partitions,
        expanded_size = image_deps.get("expanded_size", default = None),
        tags = ["manual"],
        target_compatible_with = [
            "@platforms//os:linux",
        ],
    )

    zstd_compress(
        name = "disk-img.tar.zst",
        srcs = [":disk-img.tar"],
        visibility = visibility,
        tags = ["manual"],
    )

    sha256sum(
        name = "disk-img.tar.zst.sha256",
        srcs = [":disk-img.tar.zst"],
        visibility = visibility,
        tags = ["manual"],
    )

    sha256sum2url(
        name = "disk-img.tar.zst.cas-url",
        src = ":disk-img.tar.zst.sha256",
        visibility = visibility,
        tags = ["manual"],
    )

    gzip_compress(
        name = "disk-img.tar.gz",
        srcs = [":disk-img.tar"],
        visibility = visibility,
        tags = ["manual"],
    )

    sha256sum(
        name = "disk-img.tar.gz.sha256",
        srcs = [":disk-img.tar.gz"],
        visibility = visibility,
        tags = ["manual"],
    )

    # -------------------- Assemble upgrade image --------------------

    if upgrades:
        upgrade_image(
            name = "update-img.tar",
            boot_partition = ":partition-boot.tar",
            root_partition = ":partition-root.tar",
            tags = ["manual"],
            target_compatible_with = [
                "@platforms//os:linux",
            ],
            version_file = ":version.txt",
        )

        zstd_compress(
            name = "update-img.tar.zst",
            srcs = [":update-img.tar"],
            visibility = visibility,
            tags = ["manual"],
        )

        sha256sum(
            name = "update-img.tar.zst.sha256",
            srcs = [":update-img.tar.zst"],
            visibility = visibility,
            tags = ["manual"],
        )

        sha256sum2url(
            name = "update-img.tar.zst.cas-url",
            src = ":update-img.tar.zst.sha256",
            visibility = visibility,
            tags = ["manual"],
        )

        gzip_compress(
            name = "update-img.tar.gz",
            srcs = [":update-img.tar"],
            visibility = visibility,
            tags = ["manual"],
        )

        sha256sum(
            name = "update-img.tar.gz.sha256",
            srcs = [":update-img.tar.gz"],
            visibility = visibility,
            tags = ["manual"],
        )

        upgrade_image(
            name = "update-img-test.tar",
            boot_partition = ":partition-boot-test.tar",
            root_partition = ":partition-root-test.tar",
            tags = ["manual"],
            target_compatible_with = [
                "@platforms//os:linux",
            ],
            version_file = ":version-test.txt",
        )

        zstd_compress(
            name = "update-img-test.tar.zst",
            srcs = [":update-img-test.tar"],
            visibility = visibility,
            tags = ["manual"],
        )

        sha256sum(
            name = "update-img-test.tar.zst.sha256",
            srcs = [":update-img-test.tar.zst"],
            visibility = visibility,
            tags = ["manual"],
        )

        sha256sum2url(
            name = "update-img-test.tar.zst.cas-url",
            src = ":update-img-test.tar.zst.sha256",
            visibility = visibility,
            tags = ["manual"],
        )

        gzip_compress(
            name = "update-img-test.tar.gz",
            srcs = [":update-img-test.tar"],
            visibility = visibility,
            tags = ["manual"],
        )

        sha256sum(
            name = "update-img-test.tar.gz.sha256",
            srcs = [":update-img-test.tar.gz"],
            visibility = visibility,
            tags = ["manual"],
        )

    # -------------------- Upload artifacts --------------------

    upload_suffix = ""
    if mode == "dev":
        upload_suffix = "-dev"
    if malicious:
        upload_suffix += "-malicious"

    if upload_prefix != None:
        upload_artifacts(
            name = "upload_disk-img",
            inputs = [
                ":disk-img.tar.zst",
                ":disk-img.tar.gz",
            ],
            remote_subdir = upload_prefix + "/disk-img" + upload_suffix,
            visibility = visibility,
        )

        output_files(
            name = "disk-img-url",
            target = ":upload_disk-img",
            basenames = ["upload_disk-img_disk-img.tar.zst.url"],
            visibility = visibility,
            tags = ["manual"],
        )

        output_files(
            name = "disk-img-url-gz",
            target = ":upload_disk-img",
            basenames = ["upload_disk-img_disk-img.tar.gz.url"],
            visibility = visibility,
            tags = ["manual"],
        )

        if upgrades:
            upload_artifacts(
                name = "upload_update-img",
                inputs = [
                    ":update-img.tar.zst",
                    ":update-img.tar.gz",
                    ":update-img-test.tar.zst",
                    ":update-img-test.tar.gz",
                ],
                remote_subdir = upload_prefix + "/update-img" + upload_suffix,
                visibility = visibility,
            )

            output_files(
                name = "update-img-url",
                target = ":upload_update-img",
                basenames = ["upload_update-img_update-img.tar.zst.url"],
                visibility = visibility,
                tags = ["manual"],
            )

    # end if upload_prefix != None

    # -------------------- Vulnerability scanning --------------------

    if vuln_scan:
        native.sh_binary(
            name = "vuln-scan",
            srcs = ["//ic-os:vuln-scan/vuln-scan.sh"],
            data = [
                "@trivy//:trivy",
                ":rootfs-tree.tar",
                "//ic-os:vuln-scan/vuln-scan.html",
            ],
            env = {
                "trivy_path": "$(rootpath @trivy//:trivy)",
                "CONTAINER_TAR": "$(rootpaths :rootfs-tree.tar)",
                "TEMPLATE_FILE": "$(rootpath //ic-os:vuln-scan/vuln-scan.html)",
            },
            tags = ["manual"],
        )

    # -------------------- VM Developer Tools --------------------

    native.genrule(
        name = "launch-remote-vm",
        srcs = [
            "//rs/ic_os/launch-single-vm",
            ":disk-img.tar.zst.cas-url",
            ":disk-img.tar.zst.sha256",
            "//ic-os:scripts/build-bootstrap-config-image.sh",
            "//rs/tests:src/default_firewall_whitelist.conf",
            ":version.txt",
        ],
        outs = ["launch_remote_vm_script"],
        cmd = """
        BIN="$(location //rs/ic_os/launch-single-vm:launch-single-vm)"
        VERSION="$$(cat $(location :version.txt))"
        URL="$$(cat $(location :disk-img.tar.zst.cas-url))"
        SHA="$$(cat $(location :disk-img.tar.zst.sha256))"
        SCRIPT="$(location //ic-os:scripts/build-bootstrap-config-image.sh)"
        DEFAULT_FIREWALL_WHITELIST="$(location //rs/tests:src/default_firewall_whitelist.conf)"
        cat <<EOF > $@
#!/usr/bin/env bash
set -euo pipefail
cd "\\$$BUILD_WORKSPACE_DIRECTORY"
$$BIN --version "$$VERSION" --url "$$URL" --sha256 "$$SHA" --build-bootstrap-script "$$SCRIPT" --default-firewall-whitelist "$$DEFAULT_FIREWALL_WHITELIST"
EOF
        """,
        executable = True,
        tags = ["manual"],
    )

    native.genrule(
        name = "launch-local-vm",
        srcs = [
            ":disk-img.tar",
        ],
        outs = ["launch_local_vm_script"],
        cmd = """
        IMAGE="$(location :disk-img.tar)"
        cat <<EOF > $@
#!/usr/bin/env bash
set -euo pipefail
cd "\\$$BUILD_WORKSPACE_DIRECTORY"
TEMP=\\$$(mktemp -d)
CID=\\$$((\\$$RANDOM + 3))
cp $$IMAGE \\$$TEMP
cd \\$$TEMP
tar xf disk-img.tar
qemu-system-x86_64 -machine type=q35,accel=kvm -enable-kvm -nographic -m 4G -bios /usr/share/OVMF/OVMF_CODE.fd -device vhost-vsock-pci,guest-cid=\\$$CID -drive file=disk.img,format=raw,if=virtio -netdev user,id=user.0,hostfwd=tcp::2222-:22 -device virtio-net,netdev=user.0
EOF
        """,
        executable = True,
        tags = ["manual"],
    )

    # Same as above but without KVM support to run inside VMs and containers
    # VHOST for nested VMs is not configured at the moment (should be possible)
    native.genrule(
        name = "launch-local-vm-no-kvm",
        srcs = [
            ":disk-img.tar",
        ],
        outs = ["launch_local_vm_script_no_kvm"],
        cmd = """
        IMAGE="$(location :disk-img.tar)"
        cat <<EOF > $@
#!/usr/bin/env bash
set -euo pipefail
cd "\\$$BUILD_WORKSPACE_DIRECTORY"
TEMP=\\$$(mktemp -d)
CID=\\$$((\\$$RANDOM + 3))
cp $$IMAGE \\$$TEMP
cd \\$$TEMP
tar xf disk-img.tar
qemu-system-x86_64 -machine type=q35 -nographic -m 4G -bios /usr/share/OVMF/OVMF_CODE.fd -drive file=disk.img,format=raw,if=virtio -netdev user,id=user.0,hostfwd=tcp::2222-:22 -device virtio-net,netdev=user.0
EOF
        """,
        executable = True,
        tags = ["manual"],
    )

    # -------------------- final "return" target --------------------
    # The good practice is to have the last target in the macro with `name = name`.
    # This allows users to just do `bazel build //some/path:macro_instance` without need to know internals of the macro

    native.filegroup(
        name = name,
        srcs = [
            ":disk-img.tar.zst",
            ":disk-img.tar.gz",
        ] + ([
            ":update-img.tar.zst",
            ":update-img.tar.gz",
            ":update-img-test.tar.zst",
            ":update-img-test.tar.gz",
        ] if upgrades else []),
        visibility = visibility,
    )

# end def icos_build

def boundary_node_icos_build(
        name,
        image_deps_func,
        mode = None,
        sev = False,
        visibility = None,
        ic_version = "//bazel:version.txt"):
    """
    A boundary node ICOS build parameterized by mode.

    Args:
      name: Name for the generated filegroup.
      image_deps_func: Function to be used to generate image manifest
      mode: dev, or prod. If not specified, will use the value of `name`
      sev: if True, build an SEV-SNP enabled image
      visibility: See Bazel documentation
      ic_version: the label pointing to the target that returns IC version
    """
    if mode == None:
        mode = name

    image_deps = image_deps_func(mode, sev = sev)

    native.sh_binary(
        name = "vuln-scan",
        srcs = ["//ic-os:vuln-scan/vuln-scan.sh"],
        data = [
            "@trivy//:trivy",
            ":rootfs-tree.tar",
            "//ic-os:vuln-scan/vuln-scan.html",
        ],
        env = {
            "trivy_path": "$(rootpath @trivy//:trivy)",
            "CONTAINER_TAR": "$(rootpaths :rootfs-tree.tar)",
            "TEMPLATE_FILE": "$(rootpath //ic-os:vuln-scan/vuln-scan.html)",
        },
        tags = ["manual"],
    )

    build_grub_partition("partition-grub.tar")

    build_container_filesystem_config_file = Label(image_deps["build_container_filesystem_config_file"])

    build_container_filesystem(
        name = "rootfs-tree.tar",
        context_files = ["//ic-os/boundary-guestos:rootfs-files"],
        config_file = build_container_filesystem_config_file,
        target_compatible_with = ["@platforms//os:linux"],
        tags = ["manual"],
    )

    ext4_image(
        name = "partition-config.tar",
        partition_size = "100M",
        target_compatible_with = [
            "@platforms//os:linux",
        ],
        tags = ["manual"],
    )

    copy_file(
        name = "copy_version_txt",
        src = ic_version,
        out = "version.txt",
        allow_symlink = True,
        tags = ["manual"],
    )

    ext4_image(
        name = "partition-boot.tar",
        src = _dict_value_search(image_deps["rootfs"], "/"),
        # Take the dependency list declared above, and add in the "version.txt"
        # as well as the generated extra_boot_args file in the correct place.
        extra_files = {
            k: v
            for k, v in (
                image_deps["bootfs"].items() + [
                    ("version.txt", "/boot/version.txt:0644"),
                    ("extra_boot_args", "/boot/extra_boot_args:0644"),
                ]
            )
            # Skip over special entries
            if ":bootloader/extra_boot_args.template" not in k
            if v != "/"
        },
        partition_size = "1G",
        subdir = "boot/",
        target_compatible_with = [
            "@platforms//os:linux",
        ],
        tags = ["manual"],
    )

    ext4_image(
        name = "partition-root-unsigned.tar",
        src = _dict_value_search(image_deps["rootfs"], "/"),
        # Take the dependency list declared above, and add in the "version.txt"
        # at the correct place.
        extra_files = {
            k: v
            for k, v in (image_deps["rootfs"].items() + [(":version.txt", "/opt/ic/share/version.txt:0644")])
            # Skip over special entries
            if v != "/"
        },
        partition_size = "3G",
        strip_paths = [
            "/run",
            "/boot",
        ],
        tags = ["manual"],
        target_compatible_with = [
            "@platforms//os:linux",
        ],
    )

    native.genrule(
        name = "partition-root-sign",
        srcs = ["partition-root-unsigned.tar"],
        outs = ["partition-root.tar", "partition-root-hash"],
        cmd = "$(location //toolchains/sysimage:verity_sign.py) -i $< -o $(location :partition-root.tar) -r $(location partition-root-hash)",
        executable = False,
        tools = ["//toolchains/sysimage:verity_sign.py"],
        tags = ["manual"],
    )

    native.genrule(
        name = "extra_boot_args_root_hash",
        srcs = [
            "//ic-os/boundary-guestos:bootloader/extra_boot_args.template",
            ":partition-root-hash",
        ],
        outs = ["extra_boot_args"],
        cmd = "sed -e s/ROOT_HASH/$$(cat $(location :partition-root-hash))/ < $(location //ic-os/boundary-guestos:bootloader/extra_boot_args.template) > $@",
        tags = ["manual"],
    )

    disk_image(
        name = "disk-img.tar",
        layout = "//ic-os/boundary-guestos:partitions.csv",
        partitions = [
            "//ic-os/bootloader:partition-esp.tar",
            ":partition-grub.tar",
            ":partition-config.tar",
            ":partition-boot.tar",
            ":partition-root.tar",
        ],
        expanded_size = "50G",
        tags = ["manual"],
        target_compatible_with = [
            "@platforms//os:linux",
        ],
    )

    zstd_compress(
        name = "disk-img.tar.zst",
        srcs = ["disk-img.tar"],
        visibility = visibility,
        tags = ["manual"],
    )

    sha256sum(
        name = "disk-img.tar.zst.sha256",
        srcs = [":disk-img.tar.zst"],
        visibility = visibility,
        tags = ["manual"],
    )

    sha256sum2url(
        name = "disk-img.tar.zst.cas-url",
        src = ":disk-img.tar.zst.sha256",
        visibility = visibility,
        tags = ["manual"],
    )

    gzip_compress(
        name = "disk-img.tar.gz",
        srcs = ["disk-img.tar"],
        visibility = visibility,
        tags = ["manual"],
    )

    sha256sum(
        name = "disk-img.tar.gz.sha256",
        srcs = [":disk-img.tar.gz"],
        visibility = visibility,
        tags = ["manual"],
    )

    upload_suffix = ""
    if sev:
        upload_suffix += "-snp"
    if mode == "dev":
        upload_suffix += "-dev"

    upload_artifacts(
        name = "upload_disk-img",
        inputs = [
            ":disk-img.tar.zst",
            ":disk-img.tar.gz",
        ],
        remote_subdir = "boundary-os/disk-img" + upload_suffix,
        visibility = visibility,
    )

    output_files(
        name = "disk-img-url",
        target = ":upload_disk-img",
        basenames = ["upload_disk-img_disk-img.tar.zst.url"],
        visibility = visibility,
        tags = ["manual"],
    )

    native.filegroup(
        name = name,
        srcs = [":disk-img.tar.zst", ":disk-img.tar.gz"],
        visibility = visibility,
    )

# NOTE: Really, we should be using a string keyed label dict, but this is not
# a built in. Use this hack until I switch our implementation.
def _dict_value_search(dict, value):
    for k, v in dict.items():
        if v == value:
            return k

    return None
