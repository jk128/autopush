import unittest
import uuid
from datetime import datetime, timedelta

from autopush.websocket import ms_time
from boto.dynamodb2.exceptions import (
    ItemNotFound,
)
from botocore.exceptions import ClientError
from mock import Mock
import pytest

from autopush.db import (
    get_rotating_message_tablename,
    get_router_table,
    create_router_table,
    preflight_check,
    table_exists,
    Message,
    Router,
    generate_last_connect,
    make_rotating_tablename,
    _drop_table,
    _make_table,
    )
from autopush.exceptions import AutopushException
from autopush.metrics import SinkMetrics
from autopush.utils import WebPushNotification

# nose fails to import sessions correctly.
import autopush.tests

dummy_uaid = str(uuid.UUID("abad1dea00000000aabbccdd00000000"))
dummy_chid = str(uuid.UUID("deadbeef00000000decafbad00000000"))


def make_webpush_notification(uaid, chid, ttl=100):
    message_id = str(uuid.uuid4())
    return WebPushNotification(
        uaid=uuid.UUID(uaid),
        channel_id=uuid.UUID(chid),
        update_id=message_id,
        message_id=message_id,
        ttl=ttl,
    )


class DbUtilsTest(unittest.TestCase):
    def test_make_table(self):
        fake_resource = Mock()
        fake_func = Mock()
        fake_table = "DoesNotExist_{}".format(uuid.uuid4())

        _make_table(fake_func, fake_table, 5, 10, boto_resource=fake_resource)
        assert fake_func.call_args[0] == (fake_table, 5, 10, fake_resource)


class DbCheckTestCase(unittest.TestCase):
    def setUp(cls):
        cls.resource = autopush.tests.boto_resources.fetch()

    def tearDown(cls):
        autopush.tests.boto_resources.release(cls.resource)

    def test_preflight_check_fail(self):
        router = Router(get_router_table(boto_resource=self.resource),
                        SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)
        message = Message(get_rotating_message_tablename(
            boto_resource=self.resource),
            SinkMetrics(),
            resource_pool=autopush.tests.boto_resources)

        def raise_exc(*args, **kwargs):  # pragma: no cover
            raise Exception("Oops")

        router.clear_node = Mock()
        router.clear_node.side_effect = raise_exc

        with pytest.raises(Exception):
            preflight_check(message, router, self.resource)

    def test_preflight_check(self):
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)
        message = Message(get_rotating_message_tablename(
            boto_resource=self.resource),
            SinkMetrics(),
            resource_pool=autopush.tests.boto_resources)

        pf_uaid = "deadbeef00000000deadbeef01010101"
        preflight_check(message, router, pf_uaid, self.resource)
        # now check that the database reports no entries.
        _, notifs = message.fetch_messages(uuid.UUID(pf_uaid))
        assert len(notifs) == 0
        with pytest.raises(ItemNotFound):
            router.get_uaid(pf_uaid)

    def test_preflight_check_wait(self):
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)
        message = Message(get_rotating_message_tablename(
            boto_resource=self.resource),
            SinkMetrics(),
            resource_pool=autopush.tests.boto_resources)

        values = ["PENDING", "ACTIVE"]
        message.table_status = Mock(side_effect=values)

        pf_uaid = "deadbeef00000000deadbeef01010101"
        preflight_check(message, router, pf_uaid, self.resource)
        # now check that the database reports no entries.
        _, notifs = message.fetch_messages(uuid.UUID(pf_uaid))
        assert len(notifs) == 0
        with pytest.raises(ItemNotFound):
            router.get_uaid(pf_uaid)

    def test_get_month(self):
        from autopush.db import get_month
        month0 = get_month(0)
        month1 = get_month(1)
        this_month = month0.month
        next_month = 1 if this_month == 12 else this_month + 1
        assert next_month == month1.month

    def test_zero_fill_month(self):
        from autopush.db import make_rotating_tablename
        assert 'test_2016_03' == make_rotating_tablename(
            'test', date=datetime(2016, 3, 15).date())

    def test_hasher(self):
        import autopush.db as db
        db.key_hash = "SuperSikkret"
        v = db.hasher("01234567123401234123456789ABCDEF")
        assert v == ('0530bb351921e7b4be66831e4c126c6'
                     'd8f614d06cdd592cb8470f31177c8331a')
        db.key_hash = ""

    def test_normalize_id(self):
        # Note, yes, we forbid dashes in UAIDs, and we add them here.
        import autopush.db as db
        abnormal = "deadbeef00000000decafbad00000000"
        normal = "deadbeef-0000-0000-deca-fbad00000000"
        assert db.normalize_id(abnormal) == normal
        with pytest.raises(ValueError):
            db.normalize_id("invalid")
        assert db.normalize_id(abnormal.upper()) == normal


class MessageTestCase(unittest.TestCase):
    def setUp(self):
        self.resource = autopush.tests.boto_resources.fetch()
        table = get_rotating_message_tablename(boto_resource=self.resource)
        self.real_table = table
        self.uaid = str(uuid.uuid4())

    def tearDown(self):
        autopush.tests.boto_resources.release(self.resource)
        pass

    def test_register(self):
        chid = str(uuid.uuid4())

        m = get_rotating_message_tablename(boto_resource=self.resource)
        message = Message(m, SinkMetrics(),
                          resource_pool=autopush.tests.boto_resources)
        message.register_channel(self.uaid, chid)
        lm = self.resource.Table(m)
        # Verify it's in the db
        response = lm.query(
            KeyConditions={
                'uaid': {
                    'AttributeValueList': [self.uaid],
                    'ComparisonOperator': 'EQ'
                },
                'chidmessageid': {
                    'AttributeValueList': ['02'],
                    'ComparisonOperator': 'LT'
                }
            },
            ConsistentRead=True,
        )
        assert len(response.get('Items'))

    def test_unregister(self):
        chid = str(uuid.uuid4())
        m = get_rotating_message_tablename(boto_resource=self.resource)
        message = Message(m, SinkMetrics(),
                          resource_pool=autopush.tests.boto_resources)
        message.register_channel(self.uaid, chid)

        # Verify its in the db
        lm = self.resource.Table(m)
        # Verify it's in the db
        response = lm.query(
            KeyConditions={
                'uaid': {
                    'AttributeValueList': [self.uaid],
                    'ComparisonOperator': 'EQ'
                },
                'chidmessageid': {
                    'AttributeValueList': [" "],
                    'ComparisonOperator': 'EQ'
                },
            },
            ConsistentRead=True,
        )
        results = list(response.get('Items'))
        assert len(results) == 1
        assert results[0]["chids"] == {chid}

        message.unregister_channel(self.uaid, chid)

        # Verify its not in the db
        response = lm.query(
            KeyConditions={
                'uaid': {
                    'AttributeValueList': [self.uaid],
                    'ComparisonOperator': 'EQ'
                },
                'chidmessageid': {
                    'AttributeValueList': [" "],
                    'ComparisonOperator': 'EQ'
                },
            },
            ConsistentRead=True,
        )
        results = list(response.get('Items'))
        assert len(results) == 1
        assert results[0].get("chids") is None

        # Test for the very unlikely case that there's no 'chid'
        mtable = Mock()
        mtable.update_item = Mock(return_value={
            'Attributes': {'uaid': self.uaid},
            'ResponseMetaData': {}
        })
        message.table = Mock(return_value=mtable)
        r = message.unregister_channel(self.uaid, dummy_chid)
        assert r is False

    def test_all_channels(self):
        chid = str(uuid.uuid4())
        chid2 = str(uuid.uuid4())
        m = get_rotating_message_tablename(boto_resource=self.resource)
        message = Message(m, SinkMetrics(),
                          resource_pool=autopush.tests.boto_resources)
        message.register_channel(self.uaid, chid)
        message.register_channel(self.uaid, chid2)

        _, chans = message.all_channels(self.uaid)
        assert chid in chans
        assert chid2 in chans

        message.unregister_channel(self.uaid, chid2)
        _, chans = message.all_channels(self.uaid)
        assert chid2 not in chans
        assert chid in chans

    def test_all_channels_fail(self):

        m = get_rotating_message_tablename(boto_resource=self.resource)
        message = Message(m, SinkMetrics(),
                          resource_pool=autopush.tests.boto_resources)

        mtable = Mock()
        mtable.get_item.return_value = {
            "ResponseMetadata": {
                "HTTPStatusCode": 400
            },
        }
        message.table = Mock(return_value=mtable)
        res = message.all_channels(self.uaid)
        assert res == (False, set([]))

    def test_save_channels(self):
        chid = str(uuid.uuid4())
        chid2 = str(uuid.uuid4())
        m = get_rotating_message_tablename(boto_resource=self.resource)
        message = Message(m, SinkMetrics(),
                          resource_pool=autopush.tests.boto_resources)
        message.register_channel(self.uaid, chid)
        message.register_channel(self.uaid, chid2)

        exists, chans = message.all_channels(self.uaid)
        new_uaid = uuid.uuid4().hex
        message.save_channels(new_uaid, chans)
        _, new_chans = message.all_channels(new_uaid)
        assert chans == new_chans

    def test_all_channels_no_uaid(self):
        m = get_rotating_message_tablename(boto_resource=self.resource)
        message = Message(m, SinkMetrics(),
                          resource_pool=autopush.tests.boto_resources)
        exists, chans = message.all_channels(dummy_uaid)
        assert chans == set([])

    def test_message_storage(self):
        chid = str(uuid.uuid4())
        chid2 = str(uuid.uuid4())
        m = get_rotating_message_tablename(boto_resource=self.resource)
        message = Message(m, SinkMetrics(),
                          resource_pool=autopush.tests.boto_resources)
        message.register_channel(self.uaid, chid)
        message.register_channel(self.uaid, chid2)

        message.store_message(make_webpush_notification(self.uaid, chid))
        message.store_message(make_webpush_notification(self.uaid, chid))
        message.store_message(make_webpush_notification(self.uaid, chid))

        _, all_messages = message.fetch_timestamp_messages(
            uuid.UUID(self.uaid), " ")
        assert len(all_messages) == 3

    def test_message_storage_overwrite(self):
        """Test that store_message can overwrite existing messages which
        can occur in some reconnect cases but shouldn't error"""
        chid = str(uuid.uuid4())
        chid2 = str(uuid.uuid4())
        notif1 = make_webpush_notification(self.uaid, chid)
        notif2 = make_webpush_notification(self.uaid, chid)
        notif3 = make_webpush_notification(self.uaid, chid2)
        notif2.message_id = notif1.message_id
        m = get_rotating_message_tablename(boto_resource=self.resource)
        message = Message(m, SinkMetrics(),
                          resource_pool=autopush.tests.boto_resources)
        message.register_channel(self.uaid, chid)
        message.register_channel(self.uaid, chid2)

        message.store_message(notif1)
        message.store_message(notif2)
        message.store_message(notif3)

        all_messages = list(message.fetch_messages(
            uuid.UUID(self.uaid)))
        assert len(all_messages) == 2

    def test_message_delete_fail_condition(self):
        notif = make_webpush_notification(dummy_uaid, dummy_chid)
        notif.message_id = notif.update_id = dummy_uaid
        m = get_rotating_message_tablename(boto_resource=self.resource)
        message = Message(m, SinkMetrics(),
                          resource_pool=autopush.tests.boto_resources)

        def raise_condition(*args, **kwargs):
            raise ClientError({}, 'delete_item')

        m_de = Mock()
        m_de.delete_item = Mock(side_effect=raise_condition)
        message.table = Mock(return_value=m_de)
        result = message.delete_message(notif)
        assert result is False

    def test_message_rotate_table_with_date(self):
        prefix = "message" + uuid.uuid4().hex
        future = (datetime.today() + timedelta(days=32)).date()
        tbl_name = make_rotating_tablename(prefix, date=future)

        m = get_rotating_message_tablename(prefix=prefix, date=future,
                                           boto_resource=self.resource)
        assert m == tbl_name
        # Clean up the temp table.
        _drop_table(tbl_name, boto_resource=self.resource)


class RouterTestCase(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.boto_resource = autopush.tests.boto_resources.fetch()

    @classmethod
    def tearDownClass(cls):
        autopush.tests.boto_resources.release(cls.boto_resource)

    def _create_minimal_record(self):
        data = {
            "uaid": str(uuid.uuid4()),
            "router_type": "webupsh",
            "last_connect": generate_last_connect(),
            "connected_at": ms_time(),
            "current_month": datetime.today().month,
        }
        return data

    def test_drop_old_users(self):
        # First create a bunch of users
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)
        # Purge any existing users from previous runs.
        router.drop_old_users(months_ago=0)
        for _ in range(0, 53):
            router.register_user(self._create_minimal_record())

        results = router.drop_old_users(months_ago=0)
        assert list(results) == [25, 25, 3]

    def test_custom_tablename(self):
        db_name = "router_%s" % uuid.uuid4()
        assert not table_exists(db_name, boto_resource=self.boto_resource)
        create_router_table(db_name, boto_resource=self.boto_resource)
        assert table_exists(db_name, boto_resource=self.boto_resource)
        # Clean up the temp table.
        _drop_table(db_name, boto_resource=self.boto_resource)

    def test_provisioning(self):
        db_name = "router_%s" % uuid.uuid4()

        r = create_router_table(db_name, 3, 17,
                                boto_resource=self.boto_resource)
        assert r.provisioned_throughput.get('ReadCapacityUnits') == 3
        assert r.provisioned_throughput.get('WriteCapacityUnits') == 17

    def test_no_uaid_found(self):
        uaid = str(uuid.uuid4())
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)
        with pytest.raises(ItemNotFound):
            router.get_uaid(uaid)

    def test_uaid_provision_failed(self):
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)
        router.table = Mock()

        def raise_condition(*args, **kwargs):
            raise ClientError(
                {'Error': {'Code': 'ProvisionedThroughputExceededException'}},
                'mock_update_item'
            )

        mm = Mock()
        mm.get_item = Mock(side_effect=raise_condition)
        router.table = Mock(return_value=mm)
        with pytest.raises(ClientError) as ex:
            router.get_uaid(uaid="asdf")
        assert (ex.value.response['Error']['Code'] ==
                "ProvisionedThroughputExceededException")

    def test_register_user_provision_failed(self):
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)
        mm = Mock()
        mm.client = Mock()

        router.table = Mock(return_value=mm)

        def raise_condition(*args, **kwargs):
            raise ClientError(
                {'Error': {'Code': 'ProvisionedThroughputExceededException'}},
                'mock_update_item'
            )

        mm.update_item = Mock(side_effect=raise_condition)
        with pytest.raises(ClientError) as ex:
            router.register_user(dict(uaid=dummy_uaid, node_id="me",
                                      connected_at=1234,
                                      router_type="webpush"))
        assert (ex.value.response['Error']['Code'] ==
                "ProvisionedThroughputExceededException")

    def test_register_user_condition_failed(self):
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)
        router.table().meta.client = Mock()

        def raise_error(*args, **kwargs):
            raise ClientError(
                {'Error': {'Code': 'ConditionalCheckFailedException'}},
                'mock_update_item'
            )
        mm = Mock()
        mm.update_item = Mock(side_effect=raise_error)
        router.table = Mock(return_value=mm)
        res = router.register_user(dict(uaid=dummy_uaid, node_id="me",
                                        connected_at=1234,
                                        router_type="webpush"))
        assert res == (False, {})

    def test_clear_node_provision_failed(self):
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)

        def raise_condition(*args, **kwargs):
            raise ClientError(
                {'Error': {'Code': 'ProvisionedThroughputExceededException'}},
                'mock_update_item'
            )

        mm = Mock()
        mm.put_item = Mock(side_effect=raise_condition)
        router.table = Mock(return_value=mm)
        with pytest.raises(ClientError) as ex:
            router.clear_node(dict(uaid=dummy_uaid,
                                   connected_at="1234",
                                   node_id="asdf",
                                   router_type="webpush"))
        assert (ex.value.response['Error']['Code'] ==
                "ProvisionedThroughputExceededException")

    def test_clear_node_condition_failed(self):
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)

        def raise_error(*args, **kwargs):
            raise ClientError(
                {'Error': {'Code': 'ConditionalCheckFailedException'}},
                'mock_put_item'
            )

        router.table().put_item = Mock(
            side_effect=raise_error)
        res = router.clear_node(dict(uaid=dummy_uaid,
                                     connected_at="1234",
                                     node_id="asdf",
                                     router_type="webpush"))

        assert res is False

    def test_incomplete_uaid(self):
        # Older records may be incomplete. We can't inject them using normal
        # methods.
        uaid = str(uuid.uuid4())
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)
        mm = Mock()
        mm.get_item = Mock()
        mm.get_item.return_value = {
            "ResponseMetadata": {
                "HTTPStatusCode": 200
            },
            "Item": {
                "uaid": uuid.uuid4().hex
            }
        }
        mm.delete_item.return_value = {
            "ResponseMetadata": {
                "HTTPStatusCode": 200
            },
        }
        router.table = Mock(return_value=mm)
        router.drop_user = Mock()
        try:
            router.register_user(dict(uaid=uaid))
        except AutopushException:
            pass
        with pytest.raises(ItemNotFound):
            router.get_uaid(uaid)
        assert router.drop_user.called

    def test_failed_uaid(self):
        uaid = str(uuid.uuid4())
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)
        mm = Mock()
        mm.get_item = Mock()
        mm.get_item.return_value = {
            "ResponseMetadata": {
                "HTTPStatusCode": 400
            },
        }
        router.table = Mock(return_value=mm)
        router.drop_user = Mock()
        with pytest.raises(ItemNotFound):
            router.get_uaid(uaid)

    def test_save_new(self):
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)
        # Sadly, moto currently does not return an empty value like boto
        # when not updating data.
        mock_update = Mock()
        mock_update.update_item = Mock(return_value={})
        router.table = Mock(return_value=mock_update)
        result = router.register_user(dict(uaid=dummy_uaid,
                                           node_id="me",
                                           router_type="webpush",
                                           connected_at=1234))
        assert result[0] is True

    def test_save_fail(self):
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)

        def raise_condition(*args, **kwargs):
            raise ClientError(
                {'Error': {'Code': 'ConditionalCheckFailedException'}},
                'mock_update_item'
            )

        mock_update = Mock()
        mock_update.update_item = Mock(side_effect=raise_condition)
        router.table = Mock(return_value=mock_update)
        router_data = dict(uaid=dummy_uaid, node_id="asdf", connected_at=1234,
                           router_type="webpush")
        result = router.register_user(router_data)
        assert result == (False, {})

    def test_node_clear(self):
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)

        # Register a node user
        router.register_user(dict(uaid=dummy_uaid, node_id="asdf",
                                  connected_at=1234,
                                  router_type="webpush"))
        # Verify
        user = router.get_uaid(dummy_uaid)
        assert user["node_id"] == "asdf"
        assert user["connected_at"] == 1234
        assert user["router_type"] == "webpush"

        # Clear
        router.clear_node(user)

        # Verify
        user = router.get_uaid(dummy_uaid)
        assert user.get("node_id") is None
        assert user["connected_at"] == 1234
        assert user["router_type"] == "webpush"

    def test_node_clear_fail(self):
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)

        def raise_condition(*args, **kwargs):
            raise ClientError(
                {'Error': {'Code': 'ConditionalCheckFailedException'}},
                'mock_update_item'
            )

        mock_put = Mock()
        mock_put.put_item = Mock(side_effect=raise_condition)
        router.table = Mock(return_value=mock_put)
        data = dict(uaid=dummy_uaid, node_id="asdf", connected_at=1234)
        result = router.clear_node(data)
        assert result is False

    def test_drop_user(self):
        uaid = str(uuid.uuid4())
        router = Router({}, SinkMetrics(),
                        resource_pool=autopush.tests.boto_resources)
        # Register a node user
        router.register_user(dict(uaid=uaid, node_id="asdf",
                                  router_type="webpush",
                                  connected_at=1234))
        result = router.drop_user(uaid)
        assert result is True
        # Deleting already deleted record should return false.
        result = router.drop_user(uaid)
        assert result is False
