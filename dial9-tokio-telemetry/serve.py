#!/usr/bin/env python3
import http.server, os
# Serve from the dial9-viewer/ui directory (files moved from trace_viewer/)
ui_dir = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "dial9-viewer", "ui")
os.chdir(ui_dir)
print(f"Serving {ui_dir} at http://localhost:3000")
http.server.HTTPServer(("", 3000), http.server.SimpleHTTPRequestHandler).serve_forever()
