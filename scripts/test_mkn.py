import subprocess
import json
import sys
import re

def run_cmd(args, timeout=30):
    try:
        proc = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
        return proc.returncode, proc.stdout, proc.stderr
    except subprocess.TimeoutExpired as e:
        print(f"\nFAIL: Command timed out after {timeout} seconds: {' '.join(args)}")
        return -1, (e.stdout or b"").decode(errors="replace"), (e.stderr or b"").decode(errors="replace")

def run_basic_test():
    print("Running Test 1: mkn_basic_topology...")
    code, stdout, stderr = run_cmd([
        sys.executable, "scripts/mkn.py", "meerkat/tests/mkn/test_mkn_basic.json"
    ])
    
    print(stdout)
    print(stderr)
    
    if code != 0:
        print("FAIL: Basic topology test exited with code", code)
        return False
        
    # Check that basic_server finished successfully (was shutdown)
    # Check that the logs of basic_server contain "All services online."
    # Since they are printed to stdout, we can search the output.
    if "All services online." not in stdout:
        print("FAIL: Expected 'All services online.' marker in orchestrator output")
        return False
        
    print("PASS: mkn_basic_topology")
    return True

def run_namespace_split_test():
    print("\nRunning Test 2: mkn_namespace_split...")
    code, stdout, stderr = run_cmd([
        sys.executable, "scripts/mkn.py", "meerkat/tests/mkn/test_mkn_gateway.json", "--dump-state"
    ])
    
    print(stdout)
    print(stderr)
    
    if code != 0:
        print("FAIL: Namespace split test exited with code", code)
        return False
        
    # Extract state dump
    marker_start = "--- STATE DUMP ---"
    marker_end = "--- END STATE DUMP ---"
    if marker_start not in stdout or marker_end not in stdout:
        print("FAIL: State dump not found in output")
        return False
        
    state_str = stdout.split(marker_start)[1].split(marker_end)[0].strip()
    try:
        state = json.loads(state_str)
    except Exception as e:
        print("FAIL: Failed to parse state dump JSON:", e)
        return False
        
    gateway = state.get("gateway_node")
    client = state.get("relayed_client")
    
    if not gateway or not client:
        print("FAIL: gateway_node or relayed_client missing from state dump")
        return False
        
    # Check gateway local_services
    if "gateway_svc" not in gateway.get("local_services", {}):
        print("FAIL: gateway_svc missing from gateway local_services")
        return False
        
    # Check gateway relayed_services (this verifies the three namespaces and gateway tracking!)
    relayed_services = gateway.get("relayed_services", {})
    if "client_svc" not in relayed_services:
        print("FAIL: client_svc missing from gateway relayed_services (gateway proxy tracking failed)")
        return False
        
    # Check service properties
    client_svc = relayed_services["client_svc"]
    if not client_svc.get("is_relayed"):
        print("FAIL: client_svc is_relayed is false, expected true")
        return False
        
    if client_svc.get("gateway_peer_id") != gateway.get("peer_id"):
        print(f"FAIL: client_svc gateway_peer_id ({client_svc.get('gateway_peer_id')}) does not match gateway's peer_id ({gateway.get('peer_id')})")
        return False
        
    # Check client remote_services
    if "gateway_svc" not in client.get("remote_services", {}):
        print("FAIL: gateway_svc missing from client remote_services")
        return False
        
    print("PASS: mkn_namespace_split (all 3 namespaces verified)")
    return True

def run_validation_failure_test():
    print("\nRunning Test 3: mkn_validation_failure...")
    
    test_cases = [
        ("Invalid port", "test_mkn_invalid_port.json", "cannot specify a port"),
        ("Missing alias", "test_mkn_missing_alias.json", "missing 'alias'"),
        ("Empty nodes list", "test_mkn_empty_nodes.json", "'nodes' list cannot be empty"),
        ("Duplicate alias", "test_mkn_duplicate_alias.json", "Duplicate node alias detected"),
        ("Invalid alias format", "test_mkn_invalid_alias_format.json", "must match alphanumeric/underscore format"),
        ("Missing type", "test_mkn_missing_type.json", "missing required 'type' key"),
        ("Invalid type", "test_mkn_invalid_type.json", "type must be 'server' or 'client'"),
        ("Missing file or cmd", "test_mkn_missing_file_or_cmd.json", "must specify either 'file' or 'cmd'"),
        ("Invalid cmd", "test_mkn_invalid_cmd.json", "'cmd' must be a list of strings"),
        ("Invalid port type", "test_mkn_invalid_port_type.json", "'port' must be an integer"),
        ("Server with gateway", "test_mkn_server_gateway.json", "cannot specify a gateway"),
        ("Invalid gateway reference", "test_mkn_invalid_gateway.json", "which does not exist in the manifest"),
        ("Invalid imports format", "test_mkn_invalid_imports_format.json", "must use 'alias.service_name' dot-notation"),
        ("Invalid imports reference", "test_mkn_invalid_imports_reference.json", "imports from node 'missing' which does not exist"),
        ("Circular dependency", "test_mkn_circular_dependency.json", "Circular dependency detected in manifest"),
    ]
    
    for name, filename, expected_error in test_cases:
        filepath = f"meerkat/tests/mkn/{filename}"
        code, stdout, stderr = run_cmd([sys.executable, "scripts/mkn.py", filepath])
        
        if code == 0:
            print(f"FAIL: Expected non-zero code for {name}")
            print(f"Actual output: {stdout.strip()}")
            return False
            
        if expected_error not in stdout:
            print(f"FAIL: Expected validation error regarding '{expected_error}'")
            print(f"Actual output: {stdout.strip()}")
            return False
            
        print(f"PASS: {name} check")
        
    print("\nPASS: mkn_validation_failure (All 15 edge cases checked)")
    return True

def main():
    success = True
    success &= run_basic_test()
    success &= run_namespace_split_test()
    success &= run_validation_failure_test()
    
    if success:
        print("\nALL INTEGRATION TESTS PASSED SUCCESSFULLY! ✓")
        sys.exit(0)
    else:
        print("\nSOME INTEGRATION TESTS FAILED.")
        sys.exit(1)

main()
