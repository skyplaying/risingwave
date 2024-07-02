// Copyright 2024 RisingWave Labs
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

package com.risingwave.connector;

import static io.grpc.Status.*;

import com.risingwave.connector.api.sink.SinkFactory;
import com.risingwave.mock.flink.http.HttpFlinkMockSinkFactory;
import com.risingwave.mock.flink.runtime.FlinkDynamicAdapterFactory;
import com.risingwave.proto.ConnectorServiceProto;
import java.util.Optional;

public class SinkUtils {
    public static String getConnectorName(ConnectorServiceProto.SinkParam sinkParam) {
        return Optional.ofNullable(sinkParam.getPropertiesMap().get("connector"))
                .orElseThrow(
                        () -> {
                            return INVALID_ARGUMENT
                                    .withDescription("connector not specified prop map")
                                    .asRuntimeException();
                        });
    }

    public static SinkFactory getSinkFactory(String sinkName) {
        switch (sinkName) {
            case "file":
                return new FileSinkFactory();
            case "jdbc":
                return new JDBCSinkFactory();
            case "elasticsearch":
            case "opensearch":
                return new EsSinkFactory();
            case "cassandra":
                return new CassandraFactory();
            case "http":
                return new FlinkDynamicAdapterFactory(new HttpFlinkMockSinkFactory());
            default:
                throw UNIMPLEMENTED
                        .withDescription("unknown sink type: " + sinkName)
                        .asRuntimeException();
        }
    }
}
