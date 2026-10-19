package main

import (
	"archive/tar"
	"archive/zip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"path"
	"path/filepath"
	"strings"
)

type fileRecord struct {
	Path   string `json:"path"`
	Bytes  int64  `json:"bytes"`
	SHA256 string `json:"sha256"`
}
type assetSource interface {
	Open(string) (io.ReadCloser, error)
	Close() error
}
type folderSource struct {
	root string
	flat bool
}
type zipSource struct {
	file    *zip.ReadCloser
	entries map[string]*zip.File
	prefix  string
	flat    bool
}
type patchedSource struct {
	base    assetSource
	patch   *zip.ReadCloser
	entries map[string]*zip.File
}

func safeName(n string) bool {
	return n != "" && n != "." && !strings.ContainsAny(n, "\\:\r\n\x00") && !strings.HasPrefix(n, "/") && path.Clean(n) == n && n != ".." && !strings.HasPrefix(n, "../")
}
func mappedName(n string, flat bool) string {
	if !flat && strings.HasPrefix(n, "shaders/") {
		return "b/8b0b5899ed/shaders/" + strings.TrimPrefix(n, "shaders/")
	}
	return n
}
func sourceCandidates() []struct {
	prefix string
	flat   bool
} {
	return []struct {
		prefix string
		flat   bool
	}{{"", true}, {"", false}, {"mirror/playgta5.com/", false}, {"playgta5-offline/mirror/playgta5.com/", false}, {"playgta5.com/", false}}
}
func indexZip(z *zip.ReadCloser) (map[string]*zip.File, error) {
	entries := make(map[string]*zip.File)
	for _, f := range z.File {
		n := strings.TrimSuffix(f.Name, "/")
		if !safeName(n) || f.Mode()&os.ModeSymlink != 0 {
			return nil, fmt.Errorf("unsafe path in archive: %q", f.Name)
		}
		if f.FileInfo().IsDir() {
			continue
		}
		if entries[n] != nil {
			return nil, fmt.Errorf("duplicate archive entry: %s", n)
		}
		entries[n] = f
	}
	return entries, nil
}
func openBase(p string) (assetSource, error) {
	st, e := os.Stat(p)
	if e != nil {
		return nil, e
	}
	if st.IsDir() {
		root, e := filepath.EvalSymlinks(p)
		if e != nil {
			return nil, e
		}
		root, e = filepath.Abs(root)
		if e != nil {
			return nil, e
		}
		for _, c := range sourceCandidates() {
			q := filepath.Join(root, filepath.FromSlash(c.prefix))
			a, e1 := os.Stat(filepath.Join(q, "data", "manifest.json"))
			b, e2 := os.Stat(filepath.Join(q, filepath.FromSlash(mappedName("shaders/index.json", c.flat))))
			if e1 == nil && e2 == nil && !a.IsDir() && !b.IsDir() {
				return &folderSource{q, c.flat}, nil
			}
		}
		return nil, fmt.Errorf("no supported asset layout found; select the ZIP or the folder containing data and shaders")
	}
	z, e := zip.OpenReader(p)
	if e != nil {
		return nil, fmt.Errorf("open asset ZIP: %w", e)
	}
	entries, e := indexZip(z)
	if e != nil {
		z.Close()
		return nil, e
	}
	for _, c := range sourceCandidates() {
		if entries[c.prefix+"data/manifest.json"] != nil && entries[c.prefix+mappedName("shaders/index.json", c.flat)] != nil {
			return &zipSource{z, entries, c.prefix, c.flat}, nil
		}
	}
	z.Close()
	return nil, fmt.Errorf("ZIP does not contain the required game data and shader directories")
}
func (s *folderSource) Open(n string) (io.ReadCloser, error) {
	if !safeName(n) {
		return nil, fmt.Errorf("unsafe asset name")
	}
	p := filepath.Join(s.root, filepath.FromSlash(mappedName(n, s.flat)))
	resolved, e := filepath.EvalSymlinks(p)
	if e != nil {
		return nil, e
	}
	if resolved != p {
		return nil, fmt.Errorf("asset symlink rejected: %s", n)
	}
	f, e := os.Open(p)
	if e != nil {
		return nil, e
	}
	st, e := f.Stat()
	if e != nil || !st.Mode().IsRegular() {
		f.Close()
		return nil, fmt.Errorf("asset is not a regular file: %s", n)
	}
	return f, nil
}
func (s *folderSource) Close() error { return nil }
func (s *zipSource) Open(n string) (io.ReadCloser, error) {
	if !safeName(n) {
		return nil, fmt.Errorf("unsafe asset name")
	}
	f := s.entries[s.prefix+mappedName(n, s.flat)]
	if f == nil {
		return nil, fmt.Errorf("missing asset: %s", n)
	}
	return f.Open()
}
func (s *zipSource) Close() error { return s.file.Close() }
func openAssets(p, patch string) (assetSource, error) {
	base, e := openBase(p)
	if e != nil {
		return nil, e
	}
	if patch == "" {
		return base, nil
	}
	z, e := zip.OpenReader(patch)
	if e != nil {
		base.Close()
		return nil, e
	}
	entries, e := indexZip(z)
	if e != nil {
		base.Close()
		z.Close()
		return nil, e
	}
	for n := range entries {
		if !strings.HasPrefix(n, "data/") {
			base.Close()
			z.Close()
			return nil, fmt.Errorf("unexpected native patch path: %s", n)
		}
	}
	return &patchedSource{base, z, entries}, nil
}
func (s *patchedSource) Open(n string) (io.ReadCloser, error) {
	if f := s.entries[n]; f != nil {
		return f.Open()
	}
	return s.base.Open(n)
}
func (s *patchedSource) Close() error {
	e := s.base.Close()
	p := s.patch.Close()
	if e != nil {
		return e
	}
	return p
}
func validateAssets(ctx context.Context, s assetSource, files []fileRecord, progress func(int, int64)) error {
	var total int64
	for i, f := range files {
		if e := ctx.Err(); e != nil {
			return e
		}
		if !safeName(f.Path) || f.Bytes < 0 || len(f.SHA256) != 64 {
			return fmt.Errorf("invalid pinned asset manifest")
		}
		r, e := s.Open(f.Path)
		if e != nil {
			return fmt.Errorf("%s: %w", f.Path, e)
		}
		h := sha256.New()
		n, e := io.Copy(h, io.LimitReader(r, f.Bytes+1))
		ce := r.Close()
		if e != nil {
			return e
		}
		if ce != nil {
			return ce
		}
		if n != f.Bytes || hex.EncodeToString(h.Sum(nil)) != f.SHA256 {
			return fmt.Errorf("asset checksum or size failed: %s; repair the torrent download and retry", f.Path)
		}
		total += n
		if progress != nil {
			progress(i+1, total)
		}
	}
	return nil
}
func streamAssets(ctx context.Context, w io.Writer, s assetSource, files []fileRecord, progress func(int, int64)) error {
	tw := tar.NewWriter(w)
	dirs := map[string]bool{}
	var total int64
	for i, f := range files {
		if e := ctx.Err(); e != nil {
			return e
		}
		if !safeName(f.Path) {
			return fmt.Errorf("unsafe transfer path")
		}
		parts := strings.Split(path.Dir(f.Path), "/")
		for j := 1; j <= len(parts); j++ {
			d := strings.Join(parts[:j], "/")
			if d != "." && !dirs[d] {
				if e := tw.WriteHeader(&tar.Header{Name: d + "/", Mode: 0755, Typeflag: tar.TypeDir}); e != nil {
					return e
				}
				dirs[d] = true
			}
		}
		if e := tw.WriteHeader(&tar.Header{Name: f.Path, Size: f.Bytes, Mode: 0644, Typeflag: tar.TypeReg}); e != nil {
			return e
		}
		r, e := s.Open(f.Path)
		if e != nil {
			return e
		}
		h := sha256.New()
		n, e := io.CopyN(io.MultiWriter(tw, h), r, f.Bytes)
		ce := r.Close()
		if e != nil {
			return e
		}
		if ce != nil {
			return ce
		}
		if hex.EncodeToString(h.Sum(nil)) != f.SHA256 {
			return fmt.Errorf("asset changed during transfer: %s", f.Path)
		}
		total += n
		if progress != nil {
			progress(i+1, total)
		}
	}
	return tw.Close()
}
