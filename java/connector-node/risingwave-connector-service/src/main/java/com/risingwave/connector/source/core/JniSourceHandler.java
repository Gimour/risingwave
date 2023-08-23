// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

package com.risingwave.connector.source.core;

import com.risingwave.connector.api.source.CdcEngineRunner;
import com.risingwave.connector.source.common.DbzConnectorConfig;
import com.risingwave.java.binding.Binding;
import com.risingwave.metrics.ConnectorNodeMetrics;
import io.grpc.Context;
import java.util.concurrent.TimeUnit;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/** handler for starting a debezium source connectors */
public class JniSourceHandler {
    static final Logger LOG = LoggerFactory.getLogger(DbzSourceHandler.class);

    private final DbzConnectorConfig config;

    public JniSourceHandler(DbzConnectorConfig config) {
        this.config = config;
    }

    class OnReadyHandler implements Runnable {
        private final CdcEngineRunner runner;
        private final int channelId;

        public OnReadyHandler(CdcEngineRunner runner, int channelId) {
            this.runner = runner;
            this.channelId = channelId;
        }

        @Override
        public void run() {
            while (runner.isRunning()) {
                try {
                    if (Context.current().isCancelled()) {
                        LOG.info(
                                "Engine#{}: Connection broken detected, stop the engine",
                                config.getSourceId());
                        runner.stop();
                        return;
                    }

                    // check whether the send queue has room for new messages
                    // Thread will block on the channel to get output from engine
                    var resp =
                            runner.getEngine().getOutputChannel().poll(500, TimeUnit.MILLISECONDS);
                    if (resp != null) {
                        ConnectorNodeMetrics.incSourceRowsReceived(
                                config.getSourceType().toString(),
                                String.valueOf(config.getSourceId()),
                                resp.getEventsCount());
                        LOG.debug(
                                "Engine#{}: emit one chunk {} events to network ",
                                config.getSourceId(),
                                resp.getEventsCount());

                        Binding.sendMsgToChannel(channelId, resp);
                    }
                } catch (Exception e) {
                    LOG.error("Poll engine output channel fail. ", e);
                }
            }
        }
    }

    public void start(int channelId) {
        var runner = DbzCdcEngineRunner.newCdcEngineRunnerV2(config);
        if (runner == null) {
            return;
        }

        try {
            // Start the engine
            runner.start();
            LOG.info("Start consuming events of table {}", config.getSourceId());

            final OnReadyHandler onReadyHandler = new OnReadyHandler(runner, channelId);

            onReadyHandler.run();

        } catch (Throwable t) {
            LOG.error("Cdc engine failed.", t);
            try {
                runner.stop();
            } catch (Exception e) {
                LOG.warn("Failed to stop Engine#{}", config.getSourceId(), e);
            }
        }
    }
}
