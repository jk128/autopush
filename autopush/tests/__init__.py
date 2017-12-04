import logging
import os
import signal
import subprocess

import boto
import psutil
from twisted.internet import reactor

from autopush.db import create_rotating_message_table, BotoResources

here_dir = os.path.abspath(os.path.dirname(__file__))
root_dir = os.path.dirname(os.path.dirname(here_dir))
ddb_dir = os.path.join(root_dir, "ddb")
ddb_lib_dir = os.path.join(ddb_dir, "DynamoDBLocal_lib")
ddb_jar = os.path.join(ddb_dir, "DynamoDBLocal.jar")
ddb_process = None
boto_resources = None


def setUp():
    for name in ('boto', 'boto3', 'botocore'):
        logging.getLogger(name).setLevel(logging.CRITICAL)
    global ddb_process, boto_resources
    cmd = " ".join([
        "java", "-Djava.library.path=%s" % ddb_lib_dir,
        "-jar", ddb_jar, "-sharedDb", "-inMemory"
    ])
    ddb_process = subprocess.Popen(cmd, shell=True, env=os.environ)
    if os.getenv("AWS_LOCAL_DYNAMODB") is None:
        os.environ["AWS_LOCAL_DYNAMODB"] = "http://127.0.0.1:8000"
    ddb_session_args = dict(
        endpoint_url=os.getenv("AWS_LOCAL_DYNAMODB"),
        aws_access_key_id="BogusKey",
        aws_secret_access_key="BogusKey",
    )
    boto_resources = BotoResources(conf=ddb_session_args)
    # Setup the necessary message tables
    message_table = os.environ.get("MESSAGE_TABLE", "message_int_test")
    with boto_resources as resource:
        create_rotating_message_table(prefix=message_table, delta=-1,
                                      boto_resource=resource)
        create_rotating_message_table(prefix=message_table,
                                      boto_resource=resource)
    pool = reactor.getThreadPool()
    pool.adjustPoolsize(minthreads=pool.max)


def tearDown():
    global ddb_process
    # This kinda sucks, but its the only way to nuke the child procs
    proc = psutil.Process(pid=ddb_process.pid)
    child_procs = proc.children(recursive=True)
    for p in [proc] + child_procs:
        os.kill(p.pid, signal.SIGTERM)
    ddb_process.wait()

    # Clear out the boto config that was loaded so the rest of the tests run
    # fine
    for section in boto.config.sections():
        boto.config.remove_section(section)


_multiprocess_shared_ = True


class MockAssist(object):
    def __init__(self, results):
        self.cur = 0
        self.max = len(results)
        self.results = results

    def __call__(self, *args, **kwargs):
        try:
            r = self.results[self.cur]
            if callable(r):
                return r()
            else:
                return r
        finally:
            if self.cur < (self.max - 1):
                self.cur += 1
