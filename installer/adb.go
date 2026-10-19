package main

import (
	"bytes"
	"context"
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"os/exec"
	"regexp"
	"strconv"
	"strings"
)

const remoteRoot = "/sdcard/Android/obb/com.algoartist.gta5native.dev"
const appComponent = "com.algoartist.gta5native.dev/com.algoartist.gta5native.MainActivity"

type device struct {
	Serial string `json:"serial"`
	State  string `json:"state"`
	Model  string `json:"model"`
}
type adbCommands interface {
	Run(context.Context, ...string) ([]byte, error)
	Pipe(context.Context, io.Reader, ...string) error
}
type adbTool struct{ path string }

func (a adbTool) Run(ctx context.Context, args ...string) ([]byte, error) {
	b, e := exec.CommandContext(ctx, a.path, args...).CombinedOutput()
	if e != nil {
		return b, fmt.Errorf("ADB failed: %s: %w", strings.TrimSpace(string(b)), e)
	}
	return b, nil
}
func (a adbTool) Pipe(ctx context.Context, r io.Reader, args ...string) error {
	c := exec.CommandContext(ctx, a.path, args...)
	var b bytes.Buffer
	c.Stdin = r
	c.Stdout = &b
	c.Stderr = &b
	if e := c.Run(); e != nil {
		return fmt.Errorf("ADB transfer failed: %s: %w", strings.TrimSpace(b.String()), e)
	}
	return nil
}
func parseDevices(b []byte) []device {
	result := []device{}
	for _, line := range strings.Split(string(b), "\n") {
		f := strings.Fields(line)
		if len(f) < 2 || (f[1] != "device" && f[1] != "unauthorized" && f[1] != "offline") {
			continue
		}
		d := device{Serial: f[0], State: f[1]}
		for _, s := range f[2:] {
			if strings.HasPrefix(s, "model:") {
				d.Model = strings.TrimPrefix(s, "model:")
			}
		}
		result = append(result, d)
	}
	return result
}
func dfAvailable(b []byte) (int64, error) {
	lines := strings.Split(strings.TrimSpace(string(b)), "\n")
	if len(lines) < 2 {
		return 0, fmt.Errorf("could not read device storage")
	}
	f := strings.Fields(lines[len(lines)-1])
	if len(f) < 4 {
		return 0, fmt.Errorf("unexpected device storage response")
	}
	n, e := strconv.ParseInt(f[3], 10, 64)
	if e != nil || n < 0 || n > 1<<50 {
		return 0, fmt.Errorf("invalid free-space value")
	}
	return n * 1024, nil
}
func remoteHashes(ctx context.Context, a adbCommands, serial string) (map[string]string, error) {
	script := "if [ -d '" + remoteRoot + "' ]; then cd '" + remoteRoot + "' || exit; for d in data shaders; do if [ -d \"$d\" ]; then find \"$d\" -type f -exec sha256sum {} \\; || exit; fi; done; fi"
	b, e := a.Run(ctx, "-s", serial, "shell", "-T", script)
	if e != nil {
		return nil, e
	}
	result := map[string]string{}
	for _, line := range strings.Split(strings.ReplaceAll(string(b), "\r", ""), "\n") {
		if strings.TrimSpace(line) == "" {
			continue
		}
		if len(line) < 67 || line[64:66] != "  " {
			return nil, fmt.Errorf("invalid device checksum response")
		}
		h, n := line[:64], line[66:]
		if _, e := hex.DecodeString(h); e != nil || !safeName(n) {
			return nil, fmt.Errorf("invalid device checksum record")
		}
		result[n] = h
	}
	return result, nil
}
func runInstall(ctx context.Context, a adbCommands, serial, apk string, s assetSource, files []fileRecord, log func(string)) error {
	emit := func(m string) {
		if log != nil {
			log(m)
		}
	}
	emit("Validating all local asset checksums. This can take several minutes.")
	if e := validateAssets(ctx, s, files, func(n int, b int64) {
		if n%128 == 0 || n == len(files) {
			emit(fmt.Sprintf("Validated %d / %d files (%.2f GB).", n, len(files), float64(b)/1e9))
		}
	}); e != nil {
		return e
	}
	if !regexp.MustCompile("^[A-Za-z0-9._:-]{1,128}$").MatchString(serial) {
		return fmt.Errorf("select the intended phone before installing")
	}
	b, e := a.Run(ctx, "devices", "-l")
	if e != nil {
		return e
	}
	found := false
	for _, d := range parseDevices(b) {
		if d.Serial == serial {
			if d.State != "device" {
				return fmt.Errorf("unlock the selected phone and approve USB debugging; device state is %s", d.State)
			}
			found = true
		}
	}
	if !found {
		return fmt.Errorf("selected phone is not connected")
	}
	b, e = a.Run(ctx, "-s", serial, "shell", "getprop", "ro.product.cpu.abi")
	if e != nil {
		return e
	}
	if strings.TrimSpace(string(b)) != "arm64-v8a" {
		return fmt.Errorf("this APK requires an Android ARM64 phone")
	}
	b, e = a.Run(ctx, "-s", serial, "shell", "getprop", "ro.build.version.sdk")
	if e != nil {
		return e
	}
	sdk, e := strconv.Atoi(strings.TrimSpace(string(b)))
	if e != nil || sdk < 31 {
		return fmt.Errorf("Android 12 or later is required")
	}
	b, e = a.Run(ctx, "-s", serial, "shell", "pm", "list", "features")
	if e != nil {
		return e
	}
	if !strings.Contains(string(b), "android.hardware.vulkan.version=") {
		return fmt.Errorf("the selected phone does not report Vulkan support")
	}
	emit("Checking existing phone files so verified copies can be reused.")
	existing, e := remoteHashes(ctx, a, serial)
	if e != nil {
		return e
	}
	pending := []fileRecord{}
	var needed int64
	for _, f := range files {
		if existing[f.Path] != f.SHA256 {
			pending = append(pending, f)
			needed += f.Bytes
		}
	}
	b, e = a.Run(ctx, "-s", serial, "shell", "df", "-k", "/sdcard")
	if e != nil {
		return e
	}
	free, e := dfAvailable(b)
	if e != nil {
		return e
	}
	reserve := int64(512 * 1024 * 1024)
	if st, e := os.Stat(apk); e == nil {
		reserve += st.Size() * 2
	}
	if free < needed+reserve {
		return fmt.Errorf("not enough phone storage: need %.2f GB free for remaining files and installation, have %.2f GB", float64(needed+reserve)/1e9, float64(free)/1e9)
	}
	emit("Installing GTA V without uninstalling the existing app or clearing saves.")
	b, e = a.Run(ctx, "-s", serial, "install", "-r", apk)
	if e != nil {
		return fmt.Errorf("APK update failed; the existing app and saves have not been removed: %w", e)
	}
	if !strings.Contains(string(b), "Success") {
		return fmt.Errorf("ADB did not confirm APK installation: %s", b)
	}
	if len(pending) > 0 {
		emit(fmt.Sprintf("Transferring %d missing or changed files (%.2f GB).", len(pending), float64(needed)/1e9))
		if _, e = a.Run(ctx, "-s", serial, "shell", "mkdir", "-p", remoteRoot+"/data", remoteRoot+"/shaders"); e != nil {
			return e
		}
		pr, pw := io.Pipe()
		done := make(chan error, 1)
		go func() {
			e := streamAssets(ctx, pw, s, pending, func(n int, b int64) {
				if n%64 == 0 || n == len(pending) {
					emit(fmt.Sprintf("Transferred %d / %d files (%.2f GB).", n, len(pending), float64(b)/1e9))
				}
			})
			pw.CloseWithError(e)
			done <- e
		}()
		transferErr := a.Pipe(ctx, pr, "-s", serial, "shell", "-T", "tar", "-xf", "-", "-C", remoteRoot)
		pr.CloseWithError(transferErr)
		writeErr := <-done
		if transferErr != nil {
			return transferErr
		}
		if writeErr != nil {
			return writeErr
		}
	} else {
		emit("All phone assets already match this version.")
	}
	emit("Verifying every installed asset on the phone. This can take several minutes.")
	final, e := remoteHashes(ctx, a, serial)
	if e != nil {
		return e
	}
	for _, f := range files {
		if final[f.Path] != f.SHA256 {
			return fmt.Errorf("phone checksum failed: %s; reconnect and retry to resume", f.Path)
		}
	}
	emit("Installation verified. GTA V is ready to open.")
	return nil
}
