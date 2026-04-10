#!/usr/bin/env python3
import http.server, os
os.chdir(os.path.join(os.path.dirname(os.path.abspath(__file__)), "ui"))
print("Serving at http://localhost:3001")
http.server.HTTPServer(("", 3001), http.server.SimpleHTTPRequestHandler).serve_forever()
