package main

import (
	"archive/zip"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"embed"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"time"
)

//go:embed manifest.json release.json ui.html
var bundled embed.FS

type artifact struct {
	Name   string `json:"name"`
	URL    string `json:"url"`
	SHA256 string `json:"sha256"`
	Bytes  int64  `json:"bytes"`
}
type releaseMetadata struct {
	Version    string   `json:"version"`
	Repository string   `json:"repository"`
	APK        artifact `json:"apk"`
	Patches    artifact `json:"patches"`
}
type installRequest struct {
	AssetPath string `json:"asset_path"`
	APKPath   string `json:"apk_path"`
	ADBPath   string `json:"adb_path"`
	Serial    string `json:"serial"`
}
type jobState struct {
	Running   bool     `json:"running"`
	Installed bool     `json:"installed"`
	Error     string   `json:"error"`
	Logs      []string `json:"logs"`
}
type setupApp struct {
	host, token, cache, defaultADB string
	release                        releaseMetadata
	files                          []fileRecord
	mu                             sync.Mutex
	state                          jobState
	lastSerial, lastADB            string
	shutdown                       func()
}

func hashFile(p string) (string, error) {
	f, e := os.Open(p)
	if e != nil {
		return "", e
	}
	defer f.Close()
	h := sha256.New()
	if _, e = io.Copy(h, f); e != nil {
		return "", e
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}
func fetchFile(ctx context.Context, address, expected, dest string, max int64) error {
	if expected != "" {
		if len(expected) != 64 {
			return fmt.Errorf("invalid pinned artifact hash")
		}
		if h, e := hashFile(dest); e == nil && h == expected {
			return nil
		}
	}
	u, e := url.Parse(address)
	if e != nil {
		return e
	}
	if expected == "" && (u.Scheme != "https" || u.Hostname() != "dl.google.com") {
		return fmt.Errorf("unverified downloads are allowed only from official Android Platform Tools")
	}
	if e = os.MkdirAll(filepath.Dir(dest), 0755); e != nil {
		return e
	}
	req, e := http.NewRequestWithContext(ctx, "GET", address, nil)
	if e != nil {
		return e
	}
	resp, e := http.DefaultClient.Do(req)
	if e != nil {
		return e
	}
	defer resp.Body.Close()
	if resp.StatusCode != 200 {
		return fmt.Errorf("download failed: HTTP %d for %s", resp.StatusCode, u.Path)
	}
	if resp.ContentLength > max {
		return fmt.Errorf("download exceeds its size limit")
	}
	f, e := os.CreateTemp(filepath.Dir(dest), ".download-*")
	if e != nil {
		return e
	}
	tmp := f.Name()
	defer os.Remove(tmp)
	h := sha256.New()
	n, e := io.Copy(io.MultiWriter(f, h), io.LimitReader(resp.Body, max+1))
	ce := f.Close()
	if e != nil {
		return e
	}
	if ce != nil {
		return ce
	}
	if n > max {
		return fmt.Errorf("download exceeds its size limit")
	}
	if expected != "" && hex.EncodeToString(h.Sum(nil)) != expected {
		return fmt.Errorf("download checksum failed; nothing was installed")
	}
	if e = os.Remove(dest); e != nil && !os.IsNotExist(e) {
		return e
	}
	return os.Rename(tmp, dest)
}
func newSetupApp(cache string) *setupApp {
	b := make([]byte, 32)
	if _, e := rand.Read(b); e != nil {
		panic(e)
	}
	a := &setupApp{token: hex.EncodeToString(b), cache: cache, state: jobState{Logs: []string{"Ready. Select the asset ZIP or folder, then choose your phone."}}}
	rb, e := bundled.ReadFile("release.json")
	if e != nil {
		panic(e)
	}
	if e = json.Unmarshal(rb, &a.release); e != nil {
		panic(e)
	}
	mb, e := bundled.ReadFile("manifest.json")
	if e != nil {
		panic(e)
	}
	var m struct {
		Files []fileRecord `json:"files"`
	}
	if e = json.Unmarshal(mb, &m); e != nil {
		panic(e)
	}
	a.files = m.Files
	return a
}
func (a *setupApp) log(m string) {
	a.mu.Lock()
	defer a.mu.Unlock()
	a.state.Logs = append(a.state.Logs, m)
	if len(a.state.Logs) > 400 {
		a.state.Logs = a.state.Logs[len(a.state.Logs)-400:]
	}
}
func (a *setupApp) findADB(custom string) (string, error) {
	if custom == "" {
		custom = a.defaultADB
	}
	if custom != "" {
		p, e := exec.LookPath(custom)
		if e != nil {
			return "", fmt.Errorf("cannot run the selected Android USB tool: %w", e)
		}
		return p, nil
	}
	if p, e := exec.LookPath("adb"); e == nil {
		return p, nil
	}
	name := "adb"
	if runtime.GOOS == "windows" {
		name += ".exe"
	}
	p := filepath.Join(a.cache, "platform-tools", name)
	if st, e := os.Stat(p); e == nil && !st.IsDir() {
		return p, nil
	}
	return "", fmt.Errorf("Android USB tools are missing. Click Set up USB tools first")
}
func (a *setupApp) setupADB(ctx context.Context) error {
	if p, e := a.findADB(""); e == nil {
		a.log("Android USB tools are ready: " + p)
		return nil
	}
	host := map[string]string{"darwin": "darwin", "windows": "windows", "linux": "linux"}[runtime.GOOS]
	if host == "" {
		return fmt.Errorf("unsupported desktop operating system")
	}
	address := "https://dl.google.com/android/repository/platform-tools-latest-" + host + ".zip"
	p := filepath.Join(a.cache, "platform-tools-download.zip")
	a.log("Downloading Android USB tools from Google.")
	if e := fetchFile(ctx, address, "", p, 100*1024*1024); e != nil {
		return e
	}
	z, e := zip.OpenReader(p)
	if e != nil {
		return e
	}
	defer z.Close()
	entries, e := indexZip(z)
	if e != nil {
		return e
	}
	wanted := map[string]bool{"platform-tools/adb": true, "platform-tools/adb.exe": true, "platform-tools/AdbWinApi.dll": true, "platform-tools/AdbWinUsbApi.dll": true, "platform-tools/NOTICE.txt": true}
	for n, f := range entries {
		if !wanted[n] {
			continue
		}
		if f.UncompressedSize64 > 100*1024*1024 {
			return fmt.Errorf("unexpected Android tool size")
		}
		dest := filepath.Join(a.cache, filepath.FromSlash(n))
		if e = os.MkdirAll(filepath.Dir(dest), 0755); e != nil {
			return e
		}
		r, e := f.Open()
		if e != nil {
			return e
		}
		w, e := os.OpenFile(dest, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, 0755)
		if e != nil {
			r.Close()
			return e
		}
		_, e = io.Copy(w, io.LimitReader(r, 100*1024*1024+1))
		ce := w.Close()
		r.Close()
		if e != nil {
			return e
		}
		if ce != nil {
			return ce
		}
		if e = os.Chmod(dest, 0755); e != nil {
			return e
		}
	}
	p, e = a.findADB("")
	if e != nil {
		return e
	}
	a.log("Android USB tools are ready: " + p)
	return nil
}
func (a *setupApp) artifact(ctx context.Context, art artifact, provided string) (string, error) {
	if provided != "" {
		p, e := filepath.Abs(provided)
		if e != nil {
			return "", e
		}
		h, e := hashFile(p)
		if e != nil {
			return "", e
		}
		if h != art.SHA256 {
			return "", fmt.Errorf("selected %s does not match this release", art.Name)
		}
		return p, nil
	}
	candidates := []string{}
	if exe, e := os.Executable(); e == nil {
		candidates = append(candidates, filepath.Join(filepath.Dir(exe), art.Name))
	}
	p := filepath.Join(a.cache, a.release.Version, art.Name)
	candidates = append(candidates, p)
	for _, q := range candidates {
		if h, e := hashFile(q); e == nil && h == art.SHA256 {
			return q, nil
		}
	}
	a.log("Downloading and verifying " + art.Name + " from this GitHub release.")
	if e := fetchFile(ctx, art.URL, art.SHA256, p, art.Bytes+1024); e != nil {
		return "", e
	}
	return p, nil
}
func (a *setupApp) begin(work func(context.Context) error) bool {
	a.mu.Lock()
	if a.state.Running {
		a.mu.Unlock()
		return false
	}
	a.state.Running = true
	a.state.Installed = false
	a.state.Error = ""
	a.mu.Unlock()
	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 4*time.Hour)
		defer cancel()
		e := work(ctx)
		a.mu.Lock()
		a.state.Running = false
		if e != nil {
			a.state.Error = e.Error()
			a.state.Logs = append(a.state.Logs, "Stopped: "+e.Error())
		}
		a.mu.Unlock()
	}()
	return true
}
func (a *setupApp) handler() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("X-Frame-Options", "DENY")
		w.Header().Set("X-Content-Type-Options", "nosniff")
		w.Header().Set("Cache-Control", "no-store")
		if r.Host != a.host {
			http.Error(w, "invalid host", http.StatusForbidden)
			return
		}
		if r.URL.Path == "/" && r.Method == "GET" {
			b, _ := bundled.ReadFile("ui.html")
			page := strings.ReplaceAll(strings.ReplaceAll(string(b), "__TOKEN__", a.token), "__VERSION__", a.release.Version)
			w.Header().Set("Content-Type", "text/html; charset=utf-8")
			io.WriteString(w, page)
			return
		}
		if !strings.HasPrefix(r.URL.Path, "/api/") {
			http.NotFound(w, r)
			return
		}
		if r.Header.Get("X-Setup-Token") != a.token {
			http.Error(w, "missing session token", http.StatusForbidden)
			return
		}
		if o := r.Header.Get("Origin"); o != "" && o != "http://"+a.host {
			http.Error(w, "foreign origin", http.StatusForbidden)
			return
		}
		reply := func(v any) { w.Header().Set("Content-Type", "application/json"); json.NewEncoder(w).Encode(v) }
		fail := func(e error) { http.Error(w, e.Error(), http.StatusBadRequest) }
		if r.URL.Path == "/api/state" && r.Method == "GET" {
			a.mu.Lock()
			defer a.mu.Unlock()
			reply(a.state)
			return
		}
		if r.Method != "POST" {
			http.Error(w, "POST required", http.StatusMethodNotAllowed)
			return
		}
		var q installRequest
		dec := json.NewDecoder(http.MaxBytesReader(w, r.Body, 8192))
		dec.DisallowUnknownFields()
		if e := dec.Decode(&q); e != nil {
			fail(e)
			return
		}
		switch r.URL.Path {
		case "/api/devices":
			p, e := a.findADB(q.ADBPath)
			if e != nil {
				fail(e)
				return
			}
			ctx, cancel := context.WithTimeout(r.Context(), 20*time.Second)
			defer cancel()
			b, e := (adbTool{p}).Run(ctx, "devices", "-l")
			if e != nil {
				fail(e)
				return
			}
			reply(parseDevices(b))
		case "/api/setup-adb":
			if !a.begin(a.setupADB) {
				http.Error(w, "another operation is running", 409)
				return
			}
			reply(map[string]bool{"started": true})
		case "/api/install":
			if strings.TrimSpace(q.AssetPath) == "" || q.Serial == "" {
				fail(errors.New("select the asset ZIP or folder and the intended phone"))
				return
			}
			if !a.begin(func(ctx context.Context) error {
				p, e := a.findADB(q.ADBPath)
				if e != nil {
					return e
				}
				apk, e := a.artifact(ctx, a.release.APK, q.APKPath)
				if e != nil {
					return e
				}
				patch, e := a.artifact(ctx, a.release.Patches, "")
				if e != nil {
					return e
				}
				s, e := openAssets(strings.TrimSpace(q.AssetPath), patch)
				if e != nil {
					return e
				}
				defer s.Close()
				if e = runInstall(ctx, adbTool{p}, q.Serial, apk, s, a.files, a.log); e != nil {
					return e
				}
				a.mu.Lock()
				a.state.Installed = true
				a.lastSerial = q.Serial
				a.lastADB = p
				a.mu.Unlock()
				return nil
			}) {
				http.Error(w, "another operation is running", 409)
				return
			}
			reply(map[string]bool{"started": true})
		case "/api/launch":
			a.mu.Lock()
			ok := a.state.Installed && !a.state.Running
			p, serial := a.lastADB, a.lastSerial
			a.mu.Unlock()
			if !ok {
				fail(errors.New("complete a verified installation first"))
				return
			}
			ctx, cancel := context.WithTimeout(r.Context(), 20*time.Second)
			defer cancel()
			if _, e := (adbTool{p}).Run(ctx, "-s", serial, "shell", "am", "start", "-n", appComponent); e != nil {
				fail(e)
				return
			}
			reply(map[string]bool{"launched": true})
		case "/api/quit":
			a.mu.Lock()
			busy := a.state.Running
			a.mu.Unlock()
			if busy {
				http.Error(w, "finish the current operation before quitting", 409)
				return
			}
			reply(map[string]bool{"closed": true})
			if a.shutdown != nil {
				go a.shutdown()
			}
		default:
			http.NotFound(w, r)
		}
	})
}
func main() {
	noBrowser := flag.Bool("no-browser", false, "print the local setup URL without opening a browser")
	adb := flag.String("adb", "", "optional path to the Android USB tool")
	cache := flag.String("cache", "", "optional download cache directory")
	flag.Parse()
	if *cache == "" {
		p, e := os.UserCacheDir()
		if e != nil {
			fmt.Fprintln(os.Stderr, e)
			return
		}
		*cache = filepath.Join(p, "Vulkan-GTA-V")
	}
	a := newSetupApp(*cache)
	a.defaultADB = *adb
	l, e := net.Listen("tcp4", "127.0.0.1:0")
	if e != nil {
		fmt.Fprintln(os.Stderr, e)
		return
	}
	a.host = l.Addr().String()
	server := &http.Server{Handler: a.handler(), ReadHeaderTimeout: 10 * time.Second}
	a.shutdown = func() {
		ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
		defer cancel()
		server.Shutdown(ctx)
	}
	address := "http://" + a.host + "/"
	fmt.Println("GTA V Installer: " + address)
	if !*noBrowser {
		switch runtime.GOOS {
		case "darwin":
			exec.Command("/usr/bin/open", address).Start()
		case "windows":
			exec.Command(filepath.Join(os.Getenv("SystemRoot"), "System32", "rundll32.exe"), "url.dll,FileProtocolHandler", address).Start()
		default:
			exec.Command("xdg-open", address).Start()
		}
	}
	if e = server.Serve(l); e != nil && e != http.ErrServerClosed {
		fmt.Fprintln(os.Stderr, e)
	}
}
