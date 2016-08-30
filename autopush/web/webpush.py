import time

from twisted.internet.defer import Deferred
from twisted.internet.threads import deferToThread

from autopush.web.base import (
    BaseWebHandler,
    Notification,
)
from autopush.web.validation import (
    threaded_validate,
    WebPushRequestSchema,
)
from autopush.websocket import ms_time
from autopush.db import hasher


class WebPushHandler(BaseWebHandler):
    cors_methods = "POST"
    cors_request_headers = ("content-encoding", "encryption",
                            "crypto-key", "ttl",
                            "encryption-key", "content-type",
                            "authorization")
    cors_response_headers = ("location", "www-authenticate")

    @threaded_validate(WebPushRequestSchema)
    def post(self, api_ver="v1", token=None):
        # Store Vapid info if present
        jwt = self.valid_input.get("jwt")
        if jwt:
            self._client_info["jwt_crypto_key"] = jwt["jwt_crypto_key"]
            for i in jwt["jwt_data"]:
                self._client_info["jwt_" + i] = jwt["jwt_data"][i]

        sub = self.valid_input["subscription"]
        user_data = sub["user_data"]
        router = self.ap_settings.routers[user_data["router_type"]]
        self._client_info["message_id"] = self.valid_input["message_id"]

        notification = Notification(
            version=self.valid_input["message_id"],
            data=self.valid_input["body"],
            channel_id=str(sub["chid"]),
            headers=self.valid_input["headers"],
            ttl=self.valid_input["headers"]["ttl"]
        )
        self._client_info["uaid"] = hasher(user_data.get("uaid"))
        self._client_info["channel_id"] = user_data.get("chid")
        d = Deferred()
        d.addCallback(router.route_notification, user_data)
        d.addCallback(self._router_completed, user_data, "")
        d.addErrback(self._router_fail_err)
        d.addErrback(self._response_err)

        # Call the prepared router
        d.callback(notification)

    def _router_completed(self, response, uaid_data, warning=""):
        """Called after router has completed successfully"""
        # Were we told to update the router data?
        if response.router_data is not None:
            if not response.router_data:
                # An empty router_data object indicates that the record should
                # be deleted. There is no longer valid route information for
                # this record.
                d = deferToThread(self.ap_settings.router.drop_user,
                                  uaid_data["router_data"]["uaid"])
                d.addCallback(lambda x: self._router_response(response))
                return d
            # The router data needs to be updated to include any changes
            # requested by the bridge system
            uaid_data["router_data"] = response.router_data
            # set the AWS mandatory data
            uaid_data["connected_at"] = ms_time()
            d = deferToThread(self.ap_settings.router.register_user,
                              uaid_data)
            response.router_data = None
            d.addCallback(lambda x: self._router_completed(response,
                                                           uaid_data,
                                                           warning))
            return d
        else:
            # No changes are requested by the bridge system, proceed as normal
            if response.status_code == 200 or response.logged_status == 200:
                self.log.info(format="Successful delivery",
                              client_info=self._client_info)
            elif response.status_code == 202 or response.logged_status == 202:
                self.log.info(format="Router miss, message stored.",
                              client_info=self._client_info)
            time_diff = time.time() - self.start_time
            self.metrics.timing("updates.handled", duration=time_diff)
            response.response_body = (
                response.response_body + " " + warning).strip()
            self._router_response(response)
